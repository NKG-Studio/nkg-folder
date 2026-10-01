use eframe::egui::{self, Color32, Stroke};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    #[default]
    Vscode,
    Mocha,
    Nord,
    Light,
}

impl Theme {
    pub const ALL: [Self; 4] = [Self::Vscode, Self::Mocha, Self::Nord, Self::Light];

    pub fn name(self) -> &'static str {
        match self {
            Self::Vscode => "VS Code 深色",
            Self::Mocha => "Catppuccin Mocha",
            Self::Nord => "Nord",
            Self::Light => "浅色",
        }
    }

    pub fn apply(self, ctx: &egui::Context) {
        let (base, panel, surface, input, hover, selected, text, muted, accent, border) = match self
        {
            Self::Vscode => (
                egui::Theme::Dark,
                0x181818,
                0x1f1f1f,
                0x252526,
                0x303031,
                0x264f78,
                0xcccccc,
                0x92969d,
                0x4daafc,
                0x303033,
            ),
            Self::Mocha => (
                egui::Theme::Dark,
                0x181825,
                0x1e1e2e,
                0x242436,
                0x313244,
                0x45475a,
                0xcdd6f4,
                0x9399b2,
                0x89b4fa,
                0x313244,
            ),
            Self::Nord => (
                egui::Theme::Dark,
                0x292e39,
                0x2e3440,
                0x353d4b,
                0x434c5e,
                0x4c566a,
                0xe5e9f0,
                0xa4afc0,
                0x88c0d0,
                0x434c5e,
            ),
            Self::Light => (
                egui::Theme::Light,
                0xf3f3f3,
                0xffffff,
                0xf0f1f3,
                0xe4e8ed,
                0xcce4ff,
                0x252b33,
                0x626a76,
                0x0069b5,
                0xdce0e5,
            ),
        };
        let rgb = |n: u32| Color32::from_rgb((n >> 16) as u8, (n >> 8) as u8, n as u8);
        let mut style = egui::Style {
            visuals: if base == egui::Theme::Dark {
                egui::Visuals::dark()
            } else {
                egui::Visuals::light()
            },
            ..Default::default()
        };
        let v = &mut style.visuals;
        v.override_text_color = Some(rgb(text));
        v.weak_text_color = Some(rgb(muted));
        v.panel_fill = rgb(panel);
        v.window_fill = rgb(surface);
        v.extreme_bg_color = rgb(input);
        v.faint_bg_color = rgb(surface);
        v.hyperlink_color = rgb(accent);
        v.selection.bg_fill = rgb(selected);
        v.selection.stroke = Stroke::new(1.0, rgb(accent));
        v.window_stroke = Stroke::new(1.0, rgb(border));
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.corner_radius = egui::CornerRadius::same(3);
            w.bg_stroke = Stroke::new(1.0, rgb(border));
            w.fg_stroke = Stroke::new(1.0, rgb(text));
            w.bg_fill = rgb(input);
            w.weak_bg_fill = Color32::TRANSPARENT;
        }
        v.widgets.noninteractive.bg_fill = rgb(surface);
        v.widgets.hovered.bg_fill = rgb(hover);
        v.widgets.hovered.weak_bg_fill = rgb(hover);
        v.widgets.active.bg_fill = rgb(selected);
        v.widgets.active.weak_bg_fill = rgb(selected);
        v.widgets.active.bg_stroke.color = rgb(accent);
        style.spacing.item_spacing = egui::vec2(7.0, 4.0);
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
        style.spacing.interact_size.y = 25.0;
        for text_style in [egui::TextStyle::Body, egui::TextStyle::Button] {
            style
                .text_styles
                .insert(text_style, egui::FontId::proportional(14.0));
        }
        ctx.set_theme(base);
        ctx.set_style_of(base, style);
    }
}
