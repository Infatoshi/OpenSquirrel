use gpui::{Rgba, rgba};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ThemeColors {
    pub(crate) bg: u32,
    pub(crate) surface: u32,
    pub(crate) surface_raised: u32,
    pub(crate) border: u32,
    pub(crate) border_focus: u32,
    pub(crate) text: u32,
    pub(crate) text_muted: u32,
    pub(crate) text_faint: u32,
    pub(crate) green: u32,
    pub(crate) yellow: u32,
    pub(crate) red: u32,
    pub(crate) blue: u32,
    pub(crate) blue_muted: u32,
    pub(crate) user_input: u32,
    pub(crate) palette_bg: u32,
    pub(crate) palette_border: u32,
    pub(crate) selected_row: u32,
    pub(crate) shadow: u32,
    pub(crate) glow_focus: u32,
    pub(crate) diff_add_bg: u32,
    pub(crate) diff_remove_bg: u32,
    pub(crate) diff_hunk_bg: u32,
    pub(crate) tool_call_bg: u32,
    pub(crate) tool_call_accent: u32,
    pub(crate) header_gradient_start: u32,
    pub(crate) header_gradient_end: u32,
}

impl ThemeColors {
    fn c(&self, value: u32) -> Rgba {
        rgba(value)
    }

    pub(crate) fn bg(&self) -> Rgba {
        self.c(self.bg)
    }
    pub(crate) fn surface(&self) -> Rgba {
        self.c(self.surface)
    }
    pub(crate) fn surface_raised(&self) -> Rgba {
        self.c(self.surface_raised)
    }
    pub(crate) fn border(&self) -> Rgba {
        self.c(self.border)
    }
    pub(crate) fn border_focus(&self) -> Rgba {
        self.c(self.border_focus)
    }
    pub(crate) fn text(&self) -> Rgba {
        self.c(self.text)
    }
    pub(crate) fn text_muted(&self) -> Rgba {
        self.c(self.text_muted)
    }
    pub(crate) fn text_faint(&self) -> Rgba {
        self.c(self.text_faint)
    }
    pub(crate) fn green(&self) -> Rgba {
        self.c(self.green)
    }
    pub(crate) fn yellow(&self) -> Rgba {
        self.c(self.yellow)
    }
    pub(crate) fn red(&self) -> Rgba {
        self.c(self.red)
    }
    pub(crate) fn blue(&self) -> Rgba {
        self.c(self.blue)
    }
    pub(crate) fn blue_muted(&self) -> Rgba {
        self.c(self.blue_muted)
    }
    pub(crate) fn user_input(&self) -> Rgba {
        self.c(self.user_input)
    }
    pub(crate) fn palette_bg(&self) -> Rgba {
        self.c(self.palette_bg)
    }
    pub(crate) fn palette_border(&self) -> Rgba {
        self.c(self.palette_border)
    }
    pub(crate) fn selected_row(&self) -> Rgba {
        self.c(self.selected_row)
    }
    pub(crate) fn shadow(&self) -> Rgba {
        self.c(self.shadow)
    }
    pub(crate) fn glow_focus(&self) -> Rgba {
        self.c(self.glow_focus)
    }
    pub(crate) fn diff_add_bg(&self) -> Rgba {
        self.c(self.diff_add_bg)
    }
    pub(crate) fn diff_remove_bg(&self) -> Rgba {
        self.c(self.diff_remove_bg)
    }
    pub(crate) fn diff_hunk_bg(&self) -> Rgba {
        self.c(self.diff_hunk_bg)
    }
    pub(crate) fn header_gradient_start(&self) -> Rgba {
        self.c(self.header_gradient_start)
    }
    pub(crate) fn header_gradient_end(&self) -> Rgba {
        self.c(self.header_gradient_end)
    }

    pub(crate) fn runtime_color(&self, runtime: &str) -> Rgba {
        match runtime {
            "claude" => rgba(0xE8915Aff),
            "codex" => rgba(0x7CCCF0ff),
            "cursor" => rgba(0x888888ff),
            "opencode" => rgba(0xDDDDDDff),
            "gemini" => rgba(0x4285F4ff),
            "copilot" => rgba(0x6E40C9ff),
            "terminal" => rgba(0x4EC9B0ff),
            _ => self.text_muted(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ThemeDef {
    pub(crate) name: String,
    pub(crate) colors: ThemeColors,
}

pub(crate) fn builtin_themes() -> Vec<ThemeDef> {
    vec![
        ThemeDef {
            name: "midnight".into(),
            colors: ThemeColors {
                bg: 0x0f1117ff,
                surface: 0x161922ff,
                surface_raised: 0x1c2030ff,
                border: 0x282d3eff,
                border_focus: 0x5b8aefff,
                text: 0xd4d7e0ff,
                text_muted: 0x7a8194ff,
                text_faint: 0x4a5068ff,
                green: 0x6dcb8aff,
                yellow: 0xe5c07bff,
                red: 0xe06c75ff,
                blue: 0x5b8aefff,
                blue_muted: 0x4a6bbfff,
                user_input: 0x7eb8e0ff,
                palette_bg: 0x1a1e2cff,
                palette_border: 0x3a4060ff,
                selected_row: 0x252a3cff,
                shadow: 0x00000060,
                glow_focus: 0x5b8aef30,
                diff_add_bg: 0x6dcb8a15,
                diff_remove_bg: 0xe06c7515,
                diff_hunk_bg: 0x5b8aef10,
                tool_call_bg: 0x1c203080,
                tool_call_accent: 0x5b8aefff,
                header_gradient_start: 0x161922ff,
                header_gradient_end: 0x1c2030ff,
            },
        },
        ThemeDef {
            name: "charcoal".into(),
            colors: ThemeColors {
                bg: 0x1a1a1aff,
                surface: 0x242424ff,
                surface_raised: 0x2e2e2eff,
                border: 0x3a3a3aff,
                border_focus: 0x7c9cf5ff,
                text: 0xe0e0e0ff,
                text_muted: 0x888888ff,
                text_faint: 0x555555ff,
                green: 0x7ec87eff,
                yellow: 0xd4aa4fff,
                red: 0xd46a6aff,
                blue: 0x7c9cf5ff,
                blue_muted: 0x5c7ccfff,
                user_input: 0x8fc4e8ff,
                palette_bg: 0x222222ff,
                palette_border: 0x444444ff,
                selected_row: 0x2c2c2cff,
                shadow: 0x00000060,
                glow_focus: 0x7c9cf530,
                diff_add_bg: 0x7ec87e15,
                diff_remove_bg: 0xd46a6a15,
                diff_hunk_bg: 0x7c9cf510,
                tool_call_bg: 0x2e2e2e80,
                tool_call_accent: 0x7c9cf5ff,
                header_gradient_start: 0x242424ff,
                header_gradient_end: 0x2e2e2eff,
            },
        },
        ThemeDef {
            name: "gruvbox".into(),
            colors: ThemeColors {
                bg: 0x282828ff,
                surface: 0x3c3836ff,
                surface_raised: 0x504945ff,
                border: 0x665c54ff,
                border_focus: 0x83a598ff,
                text: 0xebdbb2ff,
                text_muted: 0xa89984ff,
                text_faint: 0x7c6f64ff,
                green: 0xb8bb26ff,
                yellow: 0xfabd2fff,
                red: 0xfb4934ff,
                blue: 0x83a598ff,
                blue_muted: 0x458588ff,
                user_input: 0x8ec07cff,
                palette_bg: 0x32302fff,
                palette_border: 0x665c54ff,
                selected_row: 0x3c3836ff,
                shadow: 0x00000060,
                glow_focus: 0x83a59830,
                diff_add_bg: 0xb8bb2615,
                diff_remove_bg: 0xfb493415,
                diff_hunk_bg: 0x83a59810,
                tool_call_bg: 0x50494580,
                tool_call_accent: 0x83a598ff,
                header_gradient_start: 0x3c3836ff,
                header_gradient_end: 0x504945ff,
            },
        },
        ThemeDef {
            name: "solarized-dark".into(),
            colors: ThemeColors {
                bg: 0x002b36ff,
                surface: 0x073642ff,
                surface_raised: 0x0a4050ff,
                border: 0x586e75ff,
                border_focus: 0x268bd2ff,
                text: 0x839496ff,
                text_muted: 0x657b83ff,
                text_faint: 0x586e75ff,
                green: 0x859900ff,
                yellow: 0xb58900ff,
                red: 0xdc322fff,
                blue: 0x268bd2ff,
                blue_muted: 0x2176a8ff,
                user_input: 0x2aa198ff,
                palette_bg: 0x073642ff,
                palette_border: 0x586e75ff,
                selected_row: 0x0a4050ff,
                shadow: 0x00000060,
                glow_focus: 0x268bd230,
                diff_add_bg: 0x85990015,
                diff_remove_bg: 0xdc322f15,
                diff_hunk_bg: 0x268bd210,
                tool_call_bg: 0x0a405080,
                tool_call_accent: 0x268bd2ff,
                header_gradient_start: 0x073642ff,
                header_gradient_end: 0x0a4050ff,
            },
        },
        ThemeDef {
            name: "light".into(),
            colors: ThemeColors {
                bg: 0xf5f5f5ff,
                surface: 0xeaeaeaff,
                surface_raised: 0xe0e0e0ff,
                border: 0xccccccff,
                border_focus: 0x4078c0ff,
                text: 0x24292eff,
                text_muted: 0x586069ff,
                text_faint: 0x8b949eff,
                green: 0x22863aff,
                yellow: 0xb08800ff,
                red: 0xcb2431ff,
                blue: 0x4078c0ff,
                blue_muted: 0x6c9bd2ff,
                user_input: 0x0366d6ff,
                palette_bg: 0xf0f0f0ff,
                palette_border: 0xccccccff,
                selected_row: 0xe4e4e4ff,
                shadow: 0x00000020,
                glow_focus: 0x4078c020,
                diff_add_bg: 0x22863a12,
                diff_remove_bg: 0xcb243112,
                diff_hunk_bg: 0x4078c00c,
                tool_call_bg: 0xe0e0e080,
                tool_call_accent: 0x4078c0ff,
                header_gradient_start: 0xeaeaeaff,
                header_gradient_end: 0xe0e0e0ff,
            },
        },
        ThemeDef {
            name: "solarized-light".into(),
            colors: ThemeColors {
                bg: 0xfdf6e3ff,
                surface: 0xeee8d5ff,
                surface_raised: 0xe8e1cbff,
                border: 0xd3cbb7ff,
                border_focus: 0x268bd2ff,
                text: 0x657b83ff,
                text_muted: 0x839496ff,
                text_faint: 0x93a1a1ff,
                green: 0x859900ff,
                yellow: 0xb58900ff,
                red: 0xdc322fff,
                blue: 0x268bd2ff,
                blue_muted: 0x2176a8ff,
                user_input: 0x2aa198ff,
                palette_bg: 0xeee8d5ff,
                palette_border: 0xd3cbb7ff,
                selected_row: 0xe8e1cbff,
                shadow: 0x00000020,
                glow_focus: 0x268bd220,
                diff_add_bg: 0x85990012,
                diff_remove_bg: 0xdc322f12,
                diff_hunk_bg: 0x268bd20c,
                tool_call_bg: 0xe8e1cb80,
                tool_call_accent: 0x268bd2ff,
                header_gradient_start: 0xeee8d5ff,
                header_gradient_end: 0xe8e1cbff,
            },
        },
        ThemeDef {
            name: "ops".into(),
            colors: ThemeColors {
                bg: 0x08080cff,
                surface: 0x0e0e14ff,
                surface_raised: 0x14141cff,
                border: 0x1e1e2aff,
                border_focus: 0x44ff88ff,
                text: 0xc8ccd0ff,
                text_muted: 0x5a5e6aff,
                text_faint: 0x33364000,
                green: 0x44ff88ff,
                yellow: 0xffcc44ff,
                red: 0xff4466ff,
                blue: 0x44aaffff,
                blue_muted: 0x2277aaff,
                user_input: 0x66eeccff,
                palette_bg: 0x0c0c12ff,
                palette_border: 0x2a2a38ff,
                selected_row: 0x16162200,
                shadow: 0x00000080,
                glow_focus: 0x44ff8830,
                diff_add_bg: 0x44ff8812,
                diff_remove_bg: 0xff446612,
                diff_hunk_bg: 0x44aaff0c,
                tool_call_bg: 0x14141c80,
                tool_call_accent: 0x44aaffff,
                header_gradient_start: 0x0e0e14ff,
                header_gradient_end: 0x14141cff,
            },
        },
        ThemeDef {
            name: "monokai-pro".into(),
            colors: ThemeColors {
                bg: 0x2d2a2eff,
                surface: 0x221f22ff,
                surface_raised: 0x19181aff,
                border: 0x5b595cff,
                border_focus: 0xff6188ff,
                text: 0xfcfcfaff,
                text_muted: 0x939293ff,
                text_faint: 0x727072ff,
                green: 0xa9dc76ff,
                yellow: 0xffd866ff,
                red: 0xff6188ff,
                blue: 0x78dce8ff,
                blue_muted: 0x5ad4e6ff,
                user_input: 0xab9df2ff,
                palette_bg: 0x221f22ff,
                palette_border: 0x5b595cff,
                selected_row: 0x3a373cff,
                shadow: 0x00000070,
                glow_focus: 0xff618830,
                diff_add_bg: 0xa9dc7618,
                diff_remove_bg: 0xff618818,
                diff_hunk_bg: 0x78dce812,
                header_gradient_start: 0x221f22ff,
                header_gradient_end: 0x2d2a2eff,
                tool_call_bg: 0x19181a90,
                tool_call_accent: 0xab9df2ff,
            },
        },
    ]
}
