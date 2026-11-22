//! The composer's emoji picker, and the check that makes a message of only emoji show large.
//!
//! The list comes from the `emojis` crate (Unicode's emoji data), cut to what the window can
//! draw: egui lays out one character at a time, so emoji made of several (families, flags,
//! skin tones joined on) would show as pieces; and Noto Emoji, the bundled font, lacks the
//! newest ones. A test checks the bundled font has every emoji the picker offers.

use std::sync::OnceLock;

use eframe::egui::{self, RichText};
use emojis::{Emoji, Group, UnicodeVersion};

use super::style::palette;

/// The newest Unicode version Noto Emoji covers in full.
const NEWEST: UnicodeVersion = UnicodeVersion::new(15, 1);

/// The picker's tabs: each group with the emoji on its tab.
const GROUPS: [(Group, &str, &str); 8] = [
    (Group::SmileysAndEmotion, "😀", "Smileys"),
    (Group::PeopleAndBody, "👋", "People"),
    (Group::AnimalsAndNature, "🐻", "Animals and nature"),
    (Group::FoodAndDrink, "🍔", "Food and drink"),
    (Group::TravelAndPlaces, "✈", "Travel and places"),
    (Group::Activities, "⚽", "Activities"),
    (Group::Objects, "💡", "Objects"),
    (Group::Symbols, "❤", "Symbols"),
];

/// What the picker remembers while open.
#[derive(Default)]
pub(super) struct Picker {
    search: String,
    tab: usize,
}

/// Every emoji the picker offers, in Unicode's order.
fn offered() -> &'static [&'static Emoji] {
    static OFFERED: OnceLock<Vec<&'static Emoji>> = OnceLock::new();
    OFFERED.get_or_init(|| emojis::iter().filter(|e| drawable(e)).collect())
}

/// One character (an emoji-style selector aside), in a group the picker has, not too new.
fn drawable(emoji: &Emoji) -> bool {
    emoji.group() != Group::Flags
        && emoji.unicode_version() <= NEWEST
        && emoji.as_str().chars().filter(|c| *c != '\u{fe0f}').count() == 1
}

impl Picker {
    /// The picker's contents. Returns the emoji clicked, if one was.
    pub(super) fn show(&mut self, ui: &mut egui::Ui, recent: &[String]) -> Option<String> {
        let pal = palette(ui);
        ui.add(
            egui::TextEdit::singleline(&mut self.search)
                .hint_text("Search emoji")
                .desired_width(f32::INFINITY),
        );
        let query = self.search.trim().to_lowercase();
        if query.is_empty() {
            ui.horizontal(|ui| {
                for (i, (_, face, name)) in GROUPS.iter().enumerate() {
                    let chosen = self.tab == i;
                    if ui
                        .selectable_label(chosen, RichText::new(*face).size(18.0))
                        .on_hover_text(*name)
                        .clicked()
                    {
                        self.tab = i;
                    }
                }
            });
        }
        ui.separator();
        egui::ScrollArea::vertical()
            .max_height(230.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if !query.is_empty() {
                    let found: Vec<_> = offered().iter().filter(|e| matches(e, &query)).collect();
                    if found.is_empty() {
                        ui.label(RichText::new("No emoji found.").color(pal.weak));
                    }
                    return grid(ui, found.iter().map(|e| (e.as_str(), e.name())));
                }
                let mut picked = None;
                if !recent.is_empty() {
                    ui.label(RichText::new("Recent").small().color(pal.weak));
                    picked = grid(ui, recent.iter().map(|e| (e.as_str(), "")));
                    ui.add_space(4.0);
                }
                let (group, _, name) = GROUPS[self.tab];
                ui.label(RichText::new(name).small().color(pal.weak));
                let list = offered().iter().filter(|e| e.group() == group);
                grid(ui, list.map(|e| (e.as_str(), e.name()))).or(picked)
            })
            .inner
    }
}

/// Whether `emoji`'s name or one of its shortcodes holds `query` (lowercase).
fn matches(emoji: &Emoji, query: &str) -> bool {
    emoji.name().to_lowercase().contains(query) || emoji.shortcodes().any(|s| s.contains(query))
}

/// A wrapping grid of emoji buttons, named on hover. Returns the one clicked.
fn grid<'a>(ui: &mut egui::Ui, emoji: impl Iterator<Item = (&'a str, &'a str)>) -> Option<String> {
    let mut picked = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(2.0, 2.0);
        for (face, name) in emoji {
            let button = ui.add(
                egui::Button::new(RichText::new(face).size(22.0))
                    .frame(false)
                    .min_size(egui::vec2(32.0, 32.0)),
            );
            let button = if name.is_empty() {
                button
            } else {
                button.on_hover_text(name)
            };
            if button.clicked() {
                picked = Some(face.to_string());
            }
        }
    });
    picked
}

/// `recent` with `emoji` put first, once, and cut to the number kept.
pub(super) fn used(recent: &[String], emoji: &str) -> Vec<String> {
    std::iter::once(emoji.to_string())
        .chain(recent.iter().filter(|e| *e != emoji).cloned())
        .take(deskbeam_engine::model::RECENT_EMOJI)
        .collect()
}

/// Whether `text` is one to three emoji and nothing else (spaces aside), to show it large.
pub(super) fn only_emoji(text: &str) -> bool {
    let mut count = 0;
    for c in text.chars() {
        // Spaces, and the characters that join or style emoji: not emoji of their own.
        if c.is_whitespace()
            || matches!(
                c,
                '\u{fe0f}' | '\u{200d}' | '\u{20e3}' | '\u{1f3fb}'..='\u{1f3ff}'
            )
        {
            continue;
        }
        let one = c.to_string();
        // Digits, # and * start keycap emoji (1️⃣), but on their own they are text.
        if c.is_ascii()
            || emojis::get(&one).is_none() && emojis::get(&format!("{one}\u{fe0f}")).is_none()
        {
            return false;
        }
        count += 1;
        if count > 3 {
            return false;
        }
    }
    count > 0
}

/// Puts `text` where the cursor is in the text field `id` (at the end if it has none), and
/// gives the field the focus with the cursor after it.
pub(super) fn insert_at_cursor(ctx: &egui::Context, id: egui::Id, draft: &mut String, text: &str) {
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    let length = draft.chars().count();
    let (start, end) = match state.cursor.char_range() {
        Some(range) => {
            let (a, b) = (range.primary.index.0, range.secondary.index.0);
            (a.min(b).min(length), a.max(b).min(length))
        }
        None => (length, length),
    };
    let byte = |at: usize| draft.char_indices().nth(at).map_or(draft.len(), |(b, _)| b);
    let (from, to) = (byte(start), byte(end));
    // A selection is replaced, as typing would.
    draft.replace_range(from..to, text);
    let after = egui::text::CCursor::new(start + text.chars().count());
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::one(after)));
    state.store(ctx, id);
    ctx.memory_mut(|m| m.request_focus(id));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_font_has_every_emoji_offered() {
        use skrifa::MetadataProvider;
        let font = skrifa::FontRef::new(include_bytes!("../../assets/fonts/NotoEmoji.ttf"))
            .expect("a font");
        let charmap = font.charmap();
        let missing: Vec<&str> = offered()
            .iter()
            .filter(|e| e.as_str().chars().any(|c| charmap.map(c).is_none()))
            .map(|e| e.as_str())
            .collect();
        assert!(missing.is_empty(), "not in Noto Emoji: {missing:?}");
        assert!(
            offered().len() > 1300,
            "only {} emoji offered",
            offered().len()
        );
        // Every tab has some.
        for (group, _, name) in GROUPS {
            assert!(
                offered().iter().any(|e| e.group() == group),
                "{name} is empty"
            );
        }
    }

    #[test]
    fn only_emoji_shows_large() {
        assert!(only_emoji("😀"));
        assert!(only_emoji(" 👍🏽 ❤️ 🎉 "));
        assert!(only_emoji("👨‍👩‍👧"), "a family is three joined");
        assert!(!only_emoji("😀😀😀😀"));
        assert!(!only_emoji("hi 😀"));
        assert!(!only_emoji("123"));
        assert!(!only_emoji(""));
    }

    #[test]
    fn recent_puts_the_newest_first_once() {
        let recent = vec!["🎉".to_string(), "😀".to_string()];
        assert_eq!(used(&recent, "😀"), ["😀", "🎉"]);
        assert_eq!(used(&recent, "👍"), ["👍", "🎉", "😀"]);
        let many: Vec<String> = (0..40).map(|i| i.to_string()).collect();
        assert_eq!(used(&many, "x").len(), deskbeam_engine::model::RECENT_EMOJI);
    }
}
