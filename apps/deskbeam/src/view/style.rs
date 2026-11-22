//! The window's look: Inter for text, Lucide icons, the colours, and the few widgets every
//! page shares (icon buttons, dark and light buttons, modal cards, status dots).
//!
//! Icons are Lucide's (ISC), with the stroke set to white so a tint gives them any colour;
//! `transfer.svg` is drawn for DeskBeam in the same style. Inter is under the OFL. Both are
//! credited in NOTICE.

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, ImageSource,
    Margin, Response, RichText, Sense, Stroke, TextStyle, Theme, Vec2,
};

/// The icons the window draws.
#[derive(Debug, Clone, Copy)]
pub enum Icon {
    Chats,
    Files,
    Settings,
    Pencil,
    Copy,
    Trash,
    Plus,
    Upload,
    File,
    Folder,
    Desktop,
    Laptop,
    Check,
    CheckCheck,
    Close,
    Smile,
    Paperclip,
    Image,
    Phone,
    PhoneOff,
    Mic,
    MicOff,
    Video,
    VideoOff,
    ScreenShare,
}

impl Icon {
    pub fn source(self) -> ImageSource<'static> {
        match self {
            Self::Chats => egui::include_image!("../../assets/icons/messages-square.svg"),
            Self::Files => egui::include_image!("../../assets/icons/transfer.svg"),
            Self::Settings => egui::include_image!("../../assets/icons/settings.svg"),
            Self::Pencil => egui::include_image!("../../assets/icons/pencil.svg"),
            Self::Copy => egui::include_image!("../../assets/icons/copy.svg"),
            Self::Trash => egui::include_image!("../../assets/icons/trash-2.svg"),
            Self::Plus => egui::include_image!("../../assets/icons/plus.svg"),
            Self::Upload => egui::include_image!("../../assets/icons/upload.svg"),
            Self::File => egui::include_image!("../../assets/icons/file-text.svg"),
            Self::Folder => egui::include_image!("../../assets/icons/folder-open.svg"),
            Self::Desktop => egui::include_image!("../../assets/icons/monitor.svg"),
            Self::Laptop => egui::include_image!("../../assets/icons/laptop.svg"),
            Self::Check => egui::include_image!("../../assets/icons/check.svg"),
            Self::CheckCheck => egui::include_image!("../../assets/icons/check-check.svg"),
            Self::Close => egui::include_image!("../../assets/icons/x.svg"),
            Self::Smile => egui::include_image!("../../assets/icons/smile.svg"),
            Self::Paperclip => egui::include_image!("../../assets/icons/paperclip.svg"),
            Self::Image => egui::include_image!("../../assets/icons/image.svg"),
            Self::Phone => egui::include_image!("../../assets/icons/phone.svg"),
            Self::PhoneOff => egui::include_image!("../../assets/icons/phone-off.svg"),
            Self::Mic => egui::include_image!("../../assets/icons/mic.svg"),
            Self::MicOff => egui::include_image!("../../assets/icons/mic-off.svg"),
            Self::Video => egui::include_image!("../../assets/icons/video.svg"),
            Self::VideoOff => egui::include_image!("../../assets/icons/video-off.svg"),
            Self::ScreenShare => egui::include_image!("../../assets/icons/screen-share.svg"),
        }
    }

    /// The picture for a device, from its type ("Desktop", "Laptop", …).
    pub fn for_device(device_type: &str) -> Self {
        if device_type.to_lowercase().contains("laptop") {
            Self::Laptop
        } else {
            Self::Desktop
        }
    }
}

/// The colours of one theme.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    /// Modal cards, their bubbles, the composer.
    pub card: Color32,
    pub text: Color32,
    pub weak: Color32,
    /// Icons of the tabs not shown.
    pub idle: Color32,
    pub line: Color32,
    /// A chosen row, and this device's chat bubbles.
    pub selected: Color32,
    pub mine: Color32,
    /// Rows under the pointer, the drop zone, icon tiles.
    pub hover: Color32,
    pub dark: Color32,
    pub on_dark: Color32,
}

const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(0xf7, 0xf7, 0xf7),
    card: Color32::WHITE,
    text: Color32::from_rgb(0x1c, 0x1c, 0x1e),
    weak: Color32::from_rgb(0x86, 0x86, 0x8b),
    idle: Color32::from_rgb(0xa8, 0xa8, 0xad),
    line: Color32::from_rgb(0xd6, 0xd6, 0xd9),
    selected: Color32::from_rgb(0xcf, 0xe6, 0xfa),
    mine: Color32::from_rgb(0xc9, 0xdc, 0xf7),
    hover: Color32::from_rgb(0xec, 0xec, 0xee),
    dark: Color32::from_rgb(0x24, 0x24, 0x26),
    on_dark: Color32::WHITE,
};

const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x1e, 0x1e, 0x20),
    card: Color32::from_rgb(0x2a, 0x2a, 0x2d),
    text: Color32::from_rgb(0xf0, 0xf0, 0xf2),
    weak: Color32::from_rgb(0x98, 0x98, 0x9e),
    idle: Color32::from_rgb(0x6c, 0x6c, 0x72),
    line: Color32::from_rgb(0x3a, 0x3a, 0x3e),
    selected: Color32::from_rgb(0x1f, 0x3d, 0x5c),
    mine: Color32::from_rgb(0x24, 0x48, 0x70),
    hover: Color32::from_rgb(0x2c, 0x2c, 0x30),
    dark: Color32::from_rgb(0xf0, 0xf0, 0xf2),
    on_dark: Color32::from_rgb(0x1c, 0x1c, 0x1e),
};

pub fn palette(ui: &egui::Ui) -> Palette {
    if ui.visuals().dark_mode { DARK } else { LIGHT }
}

pub const ONLINE: Color32 = Color32::from_rgb(0x22, 0xb8, 0x5a);
pub const WARN: Color32 = Color32::from_rgb(0xd9, 0x8a, 0x1c);
pub const ERROR: Color32 = Color32::from_rgb(0xd6, 0x45, 0x45);
pub const ACCENT: Color32 = Color32::from_rgb(0x3d, 0x8b, 0xfd);

/// Inter's semibold, for headings and names.
pub fn bold() -> FontFamily {
    FontFamily::Name("bold".into())
}

/// Text in Inter semibold at `size`.
pub fn strong(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text).family(bold()).size(size)
}

/// Fonts and the two themes' styles, once at start.
pub fn install(ctx: &egui::Context) {
    egui_extras::install_image_loaders(ctx);

    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "inter".into(),
        FontData::from_static(include_bytes!("../../assets/fonts/Inter-Regular.ttf")).into(),
    );
    fonts.font_data.insert(
        "inter-semibold".into(),
        FontData::from_static(include_bytes!("../../assets/fonts/Inter-SemiBold.ttf")).into(),
    );
    // Emoji: Noto Emoji in full (egui's own has about two thirds of them), right after Inter.
    fonts.font_data.insert(
        "noto-emoji".into(),
        FontData::from_static(include_bytes!("../../assets/fonts/NotoEmoji.ttf")).into(),
    );
    // egui's own fonts stay behind these for symbols they lack.
    let fallback = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .splice(0..0, ["inter".to_string(), "noto-emoji".to_string()]);
    let mut semibold = vec!["inter-semibold".to_string(), "noto-emoji".to_string()];
    semibold.extend(fallback);
    fonts.families.insert(bold(), semibold);
    ctx.set_fonts(fonts);

    for (theme, pal) in [(Theme::Light, LIGHT), (Theme::Dark, DARK)] {
        ctx.style_mut_of(theme, |style| {
            style
                .text_styles
                .insert(TextStyle::Heading, FontId::new(18.0, bold()));
            style
                .text_styles
                .insert(TextStyle::Body, FontId::proportional(14.0));
            style
                .text_styles
                .insert(TextStyle::Button, FontId::proportional(14.0));
            style
                .text_styles
                .insert(TextStyle::Small, FontId::proportional(11.5));
            style.spacing.item_spacing = Vec2::new(8.0, 6.0);
            style.spacing.button_padding = Vec2::new(10.0, 5.0);
            style.spacing.interact_size.y = 26.0;

            let v = &mut style.visuals;
            v.panel_fill = pal.bg;
            v.window_fill = pal.card;
            v.extreme_bg_color = pal.card;
            v.faint_bg_color = pal.card;
            v.override_text_color = Some(pal.text);
            v.window_stroke = Stroke::NONE;
            v.window_corner_radius = CornerRadius::same(16);
            v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
            v.selection.stroke = Stroke::new(1.0, ACCENT);
            let round = CornerRadius::same(6);
            v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, pal.line);
            v.widgets.noninteractive.corner_radius = round;
            v.widgets.inactive.weak_bg_fill = pal.hover;
            v.widgets.inactive.bg_fill = pal.hover;
            v.widgets.inactive.corner_radius = round;
            v.widgets.hovered.weak_bg_fill = pal.line;
            v.widgets.hovered.bg_fill = pal.line;
            v.widgets.hovered.bg_stroke = Stroke::NONE;
            v.widgets.hovered.corner_radius = round;
            v.widgets.active.weak_bg_fill = pal.line;
            v.widgets.active.corner_radius = round;
            v.widgets.open.corner_radius = round;
        });
    }
}

/// An icon at `size` points, in `color`.
pub fn icon(ui: &mut egui::Ui, which: Icon, size: f32, color: Color32) -> Response {
    ui.add(
        egui::Image::new(which.source())
            .fit_to_exact_size(Vec2::splat(size))
            .tint(color),
    )
}

/// A frameless icon that darkens under the pointer.
pub fn icon_button(ui: &mut egui::Ui, which: Icon, size: f32, tip: &str) -> Response {
    let pal = palette(ui);
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size + 4.0), Sense::click());
    let color = if response.hovered() {
        pal.text
    } else {
        pal.weak
    };
    egui::Image::new(which.source())
        .tint(color)
        .paint_at(ui, rect.shrink(2.0));
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tip)
}

/// A dark, filled button, for the action a screen is about.
pub fn primary(ui: &mut egui::Ui, text: &str, enabled: bool) -> Response {
    let pal = palette(ui);
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).color(pal.on_dark).size(15.0))
            .fill(pal.dark)
            .corner_radius(10)
            .min_size(Vec2::new(0.0, 36.0)),
    )
}

/// A light button with a thin border, beside a primary one.
pub fn secondary(ui: &mut egui::Ui, text: &str) -> Response {
    let pal = palette(ui);
    ui.add(
        egui::Button::new(RichText::new(text).color(pal.text).size(15.0))
            .fill(pal.card)
            .stroke(Stroke::new(1.0, pal.line))
            .corner_radius(10)
            .min_size(Vec2::new(0.0, 36.0)),
    )
}

/// A dark button with an icon after its text, as on the Files page.
pub fn dark_icon_button(ui: &mut egui::Ui, text: &str, which: Icon, enabled: bool) -> Response {
    let pal = palette(ui);
    let image = egui::Image::new(which.source())
        .fit_to_exact_size(Vec2::splat(18.0))
        .tint(pal.on_dark);
    ui.add_enabled(
        enabled,
        egui::Button::new((RichText::new(text).color(pal.on_dark).size(15.0), image))
            .fill(pal.dark)
            .corner_radius(4)
            .min_size(Vec2::new(0.0, 38.0)),
    )
}

/// A coloured dot and a word: "● Online".
pub fn status(ui: &mut egui::Ui, online: bool) -> Response {
    let pal = palette(ui);
    let (color, text) = if online {
        (ONLINE, "Online")
    } else {
        (pal.idle, "Offline")
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        dot(ui, color, 3.5);
        ui.label(RichText::new(text).color(pal.weak));
    })
    .response
}

pub fn dot(ui: &mut egui::Ui, color: Color32, radius: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(radius * 2.0 + 2.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), radius, color);
}

/// A modal card: title, a line under it, the body, then a footer for buttons. Returns
/// whether it should close (Escape or a click outside) and the body's value.
pub fn modal<R>(
    ctx: &egui::Context,
    id: &str,
    title: &str,
    subtitle: &str,
    body: impl FnOnce(&mut egui::Ui) -> R,
    footer: impl FnOnce(&mut egui::Ui),
) -> (bool, R) {
    let frame = egui::Frame::new()
        .fill(if ctx.theme() == Theme::Dark {
            DARK.card
        } else {
            LIGHT.card
        })
        .corner_radius(16)
        .shadow(egui::epaint::Shadow {
            offset: [0, 8],
            blur: 32,
            spread: 0,
            color: Color32::from_black_alpha(40),
        });
    let response = egui::Modal::new(egui::Id::new(id))
        .frame(frame)
        .show(ctx, |ui| {
            let pal = palette(ui);
            ui.set_width(420.0);
            let inner = egui::Frame::new()
                .inner_margin(Margin {
                    left: 24,
                    right: 24,
                    top: 22,
                    bottom: 18,
                })
                .show(ui, |ui| {
                    ui.label(strong(title, 20.0));
                    if !subtitle.is_empty() {
                        ui.label(RichText::new(subtitle).color(pal.weak));
                    }
                    ui.add_space(10.0);
                    body(ui)
                })
                .inner;
            let rect = ui.max_rect();
            let footer_frame = egui::Frame::new()
                .fill(pal.bg)
                .corner_radius(CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: 16,
                    se: 16,
                })
                .inner_margin(Margin::symmetric(24, 14));
            ui.painter().hline(
                rect.x_range(),
                ui.cursor().top(),
                Stroke::new(1.0, pal.line),
            );
            footer_frame.show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), footer);
            });
            inner
        });
    (response.should_close(), response.inner)
}

/// A section's title: "This device", "Paired devices".
pub fn section(ui: &mut egui::Ui, text: &str) {
    ui.label(strong(text, 15.0));
}

/// A thin line across the available width.
pub fn rule(ui: &mut egui::Ui) {
    let pal = palette(ui);
    let rect = ui.available_rect_before_wrap();
    let y = ui.cursor().top();
    ui.painter()
        .hline(rect.x_range(), y, Stroke::new(1.0, pal.line));
    ui.add_space(1.0);
}
