//! The app's colours: a dark and a light palette laid out like an API
//! client (dark chrome, raised panels, one warm accent), applied to
//! gpui-kit's theme so its buttons and inputs match the custom panels.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Global, Hsla, rgb};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Look {
    pub dark: bool,
    /// Behind everything: header, rail, status bar.
    pub chrome: Hsla,
    /// The sidebar and the main panels.
    pub surface: Hsla,
    /// Inputs, chips and hovered rows.
    pub raised: Hsla,
    pub border: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub accent: Hsla,
    pub accent_hover: Hsla,
    pub on_accent: Hsla,
    pub success: Hsla,
    pub info: Hsla,
    pub warning: Hsla,
    pub danger: Hsla,
}

impl Global for Look {}

fn color(hex: u32) -> Hsla {
    rgb(hex).into()
}

impl Look {
    pub fn dark() -> Self {
        Self {
            dark: true,
            chrome: color(0x161618),
            surface: color(0x1E1E21),
            raised: color(0x2A2A2E),
            border: color(0x34343A),
            text: color(0xECECEF),
            muted: color(0x9A9AA4),
            accent: color(0xFF6A33),
            accent_hover: color(0xFF8355),
            on_accent: color(0xFFFFFF),
            success: color(0x3FB950),
            info: color(0x58A6FF),
            warning: color(0xD9A43A),
            danger: color(0xF0605A),
        }
    }

    pub fn light() -> Self {
        Self {
            dark: false,
            chrome: color(0xF3F3F5),
            surface: color(0xFFFFFF),
            raised: color(0xF0F0F3),
            border: color(0xE1E1E6),
            text: color(0x1E1E22),
            muted: color(0x6A6A75),
            accent: color(0xEB5424),
            accent_hover: color(0xF26A3F),
            on_accent: color(0xFFFFFF),
            success: color(0x1F8A3B),
            info: color(0x1F6FD1),
            warning: color(0xA86F00),
            danger: color(0xD1352F),
        }
    }

    pub fn global(cx: &App) -> Self {
        cx.try_global::<Self>().copied().unwrap_or_else(Self::dark)
    }

    /// Makes this the app's look, gpui-kit components included.
    pub fn apply(self, cx: &mut App) {
        let mode = if self.dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        };
        Theme::change(mode, None, cx);
        Theme::update(cx, |theme| {
            let c = &mut theme.colors;
            c.background = self.surface;
            c.foreground = self.text;
            c.border = self.border;
            c.input = self.border;
            c.muted = self.raised;
            c.muted_foreground = self.muted;
            c.accent = self.raised;
            c.accent_foreground = self.text;
            c.popover = self.surface;
            c.popover_foreground = self.text;
            c.ring = self.accent;
            c.caret = self.accent;
            c.primary = self.accent;
            c.primary_hover = self.accent_hover;
            c.primary_active = self.accent;
            c.primary_foreground = self.on_accent;
            c.button_primary = self.accent;
            c.button_primary_hover = self.accent_hover;
            c.button_primary_active = self.accent;
            c.button_primary_foreground = self.on_accent;
            c.secondary = self.raised;
            c.secondary_hover = self.border;
            c.secondary_active = self.border;
            c.secondary_foreground = self.text;
            c.progress_bar = self.accent;
            c.switch = self.border;
            c.list_hover = self.raised;
            c.danger = self.danger;
            c.success = self.success;
            c.warning = self.warning;
            c.info = self.info;
        });
        cx.set_global(self);
    }
}
