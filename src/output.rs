use std::io::{self, IsTerminal};

use crossterm::style::{Attribute, Color, Stylize, style};

pub(crate) const ACCENT: Color = Color::Rgb {
    r: 86,
    g: 182,
    b: 194,
};
pub(crate) const SUCCESS: Color = Color::Rgb {
    r: 152,
    g: 195,
    b: 121,
};
pub(crate) const WARNING: Color = Color::Rgb {
    r: 229,
    g: 192,
    b: 123,
};
pub(crate) const ERROR: Color = Color::Rgb {
    r: 224,
    g: 108,
    b: 117,
};
pub(crate) const MUTED: Color = Color::Rgb {
    r: 128,
    g: 135,
    b: 150,
};
pub(crate) const SELECTED_BG: Color = Color::Rgb {
    r: 35,
    g: 78,
    b: 86,
};
pub(crate) const SELECTED_TEXT: Color = Color::Rgb {
    r: 238,
    g: 242,
    b: 246,
};
pub(crate) const SCOPE_PALETTE: [Color; 6] = [
    Color::Rgb {
        r: 97,
        g: 175,
        b: 239,
    },
    Color::Rgb {
        r: 198,
        g: 120,
        b: 221,
    },
    Color::Rgb {
        r: 86,
        g: 182,
        b: 194,
    },
    Color::Rgb {
        r: 209,
        g: 134,
        b: 178,
    },
    Color::Rgb {
        r: 125,
        g: 130,
        b: 230,
    },
    Color::Rgb {
        r: 70,
        g: 180,
        b: 160,
    },
];

#[derive(Clone, Copy)]
pub(crate) struct HumanOutput {
    styled: bool,
}

impl HumanOutput {
    pub(crate) fn stdout() -> Self {
        Self {
            styled: color_enabled(io::stdout().is_terminal()),
        }
    }

    pub(crate) fn stderr() -> Self {
        Self {
            styled: color_enabled(io::stderr().is_terminal()),
        }
    }

    #[cfg(test)]
    pub(crate) fn plain() -> Self {
        Self { styled: false }
    }

    #[cfg(test)]
    pub(crate) fn styled() -> Self {
        Self { styled: true }
    }

    pub(crate) fn heading(self, message: &str) -> String {
        self.paint(message, ACCENT, true)
    }

    pub(crate) fn success(self, message: &str) -> String {
        self.line("✓", message, SUCCESS, None)
    }

    pub(crate) fn info(self, message: &str) -> String {
        self.line("→", message, ACCENT, None)
    }

    pub(crate) fn warning(self, message: &str) -> String {
        self.line("!", message, WARNING, Some("warning:"))
    }

    pub(crate) fn error(self, message: &str) -> String {
        self.line("×", message, ERROR, Some("error:"))
    }

    pub(crate) fn item(self, message: &str) -> String {
        self.line("•", message, ACCENT, Some("-"))
    }

    fn line(self, icon: &str, message: &str, color: Color, plain_prefix: Option<&str>) -> String {
        if self.styled {
            format!("{} {message}", self.paint(icon, color, true))
        } else if let Some(prefix) = plain_prefix {
            format!("{prefix} {message}")
        } else {
            message.to_owned()
        }
    }

    fn paint(self, message: &str, color: Color, bold: bool) -> String {
        if !self.styled {
            return message.to_owned();
        }
        let content = style(message).with(color);
        if bold {
            format!("{}", content.attribute(Attribute::Bold))
        } else {
            content.to_string()
        }
    }
}

pub(crate) fn color_enabled(terminal: bool) -> bool {
    terminal
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var_os("TERM").is_none_or(|term| term != "dumb")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_output_preserves_script_friendly_prefixes() {
        let output = HumanOutput::plain();
        assert_eq!(output.success("done"), "done");
        assert_eq!(output.info("working"), "working");
        assert_eq!(output.warning("careful"), "warning: careful");
        assert_eq!(output.error("blocked"), "error: blocked");
        assert_eq!(output.item("skill"), "- skill");
    }

    #[test]
    fn styled_output_uses_semantic_icons_and_ansi_roles() {
        let output = HumanOutput::styled();
        assert!(output.success("done").contains('✓'));
        assert!(output.success("done").contains("done"));
        assert!(output.warning("careful").contains('!'));
        assert!(output.warning("careful").contains("careful"));
        assert!(output.error("blocked").contains('×'));
        assert!(output.error("blocked").contains("blocked"));
        assert!(output.heading("Updates").contains("Updates"));
        assert!(output.success("done").contains("\u{1b}["));
    }
}
