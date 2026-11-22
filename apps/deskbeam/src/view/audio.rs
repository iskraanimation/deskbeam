//! The Settings window's Calls section: microphone and speaker, the processing tuned for
//! calls (gain, echo, noise), and a test that plays the microphone back.

use std::time::Duration;

use deskbeam_engine::{
    Command,
    model::{AudioSettings, CameraQuality, NoiseSuppression, Snapshot},
};
use eframe::egui::{self, RichText};

use super::{DeskBeamApp, call::level_meter, style::palette};

impl DeskBeamApp {
    pub(super) fn audio_settings(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let pal = palette(ui);
        if snap.audio_devices.is_none() && !self.audio_listed {
            self.audio_listed = true;
            self.dispatch(Command::ListAudioDevices);
        }
        // Shown from the draft while the engine catches up, so a dragged slider stays put.
        if self.audio_draft.as_ref() == Some(&snap.preferences.audio) {
            self.audio_draft = None;
        }
        let mut audio = self
            .audio_draft
            .clone()
            .unwrap_or_else(|| snap.preferences.audio.clone());
        let before = audio.clone();
        let devices = snap.audio_devices.clone().unwrap_or_default();

        egui::Grid::new("audio-settings")
            .num_columns(2)
            .spacing([14.0, 8.0])
            .show(ui, |ui| {
                ui.label("Microphone");
                ui.horizontal(|ui| {
                    device_choice(
                        ui,
                        "microphone",
                        &mut audio.microphone,
                        &devices.microphones,
                    );
                    if ui
                        .small_button("Refresh")
                        .on_hover_text("Look again for microphones, speakers and cameras")
                        .clicked()
                    {
                        self.dispatch(Command::ListAudioDevices);
                    }
                });
                ui.end_row();

                ui.label("Speaker");
                device_choice(ui, "speaker", &mut audio.speaker, &devices.speakers);
                ui.end_row();

                ui.label("Camera");
                device_choice(ui, "camera", &mut audio.camera, &devices.cameras);
                ui.end_row();

                ui.label("Camera quality");
                egui::ComboBox::from_id_salt("camera-quality")
                    .selected_text(audio.camera_quality.label())
                    .show_ui(ui, |ui| {
                        for quality in CameraQuality::ALL {
                            ui.selectable_value(
                                &mut audio.camera_quality,
                                quality,
                                quality.label(),
                            );
                        }
                    })
                    .response
                    .on_hover_text(
                        "HD needs a camera that can do 720p and about 2–3 times the bandwidth \
                         and CPU. It takes effect the next time the camera is turned on.",
                    );
                ui.end_row();

                ui.label("Automatic gain");
                ui.checkbox(&mut audio.auto_gain, "Raise a quiet voice by itself");
                ui.end_row();

                ui.label("Most it raises");
                ui.add_enabled(
                    audio.auto_gain,
                    egui::Slider::new(&mut audio.max_gain_db, 0..=AudioSettings::MOST_GAIN_DB)
                        .suffix(" dB"),
                )
                .on_hover_text("Lower it if background noise gets loud between words.");
                ui.end_row();

                ui.label("Microphone boost");
                ui.add(
                    egui::Slider::new(&mut audio.boost_db, 0..=AudioSettings::MOST_BOOST_DB)
                        .suffix(" dB"),
                )
                .on_hover_text(
                    "A fixed boost for a quiet microphone, mostly with automatic gain off. \
                     It never clips: a limiter follows.",
                );
                ui.end_row();

                ui.label("Echo cancellation");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut audio.echo_cancellation, "");
                    ui.label(
                        RichText::new(
                            "Keeps your speaker out of your microphone. Turn off only with \
                             headphones.",
                        )
                        .color(pal.weak),
                    );
                });
                ui.end_row();

                ui.label("Noise suppression");
                egui::ComboBox::from_id_salt("noise-suppression")
                    .selected_text(audio.noise_suppression.label())
                    .show_ui(ui, |ui| {
                        for level in NoiseSuppression::ALL {
                            ui.selectable_value(&mut audio.noise_suppression, level, level.label());
                        }
                    });
                ui.end_row();
            });

        if audio != before {
            self.audio_draft = Some(audio.clone());
            self.dispatch(Command::SetAudio { settings: audio });
        }
        if ui
            .small_button("Back to the defaults")
            .on_hover_text("The settings calls were tuned with; keeps the chosen devices")
            .clicked()
        {
            let settings = AudioSettings {
                microphone: before.microphone.clone(),
                speaker: before.speaker.clone(),
                ..Default::default()
            };
            self.audio_draft = Some(settings.clone());
            self.dispatch(Command::SetAudio { settings });
        }

        ui.add_space(8.0);
        let in_call = snap.call.as_ref().is_some_and(|c| c.is_live());
        ui.horizontal(|ui| match snap.audio_test {
            Some(level) => {
                if ui.button("Stop the test").clicked() {
                    self.dispatch(Command::TestAudio { on: false });
                }
                level_meter(ui, level);
                ui.ctx().request_repaint_after(Duration::from_millis(250));
            }
            None => {
                if ui
                    .add_enabled(!in_call, egui::Button::new("Test microphone and speaker"))
                    .on_disabled_hover_text("Not during a call")
                    .clicked()
                {
                    self.dispatch(Command::TestAudio { on: true });
                }
            }
        });
        ui.label(
            RichText::new(
                "The test plays you back for up to 15 seconds: wear headphones, or the speaker \
                 feeds into the microphone. Changes take effect at once, in a call too; a new \
                 microphone or speaker from the next call.",
            )
            .small()
            .color(pal.weak),
        );
    }
}

/// A device list: "System default" first, then the devices by name. A chosen device that
/// is not there now still shows, marked.
fn device_choice(ui: &mut egui::Ui, id: &str, chosen: &mut Option<String>, names: &[String]) {
    let shown = match chosen.as_deref() {
        None => "System default".to_string(),
        Some(name) if names.iter().any(|n| n == name) => name.to_string(),
        Some(name) => format!("{name} (not found)"),
    };
    egui::ComboBox::from_id_salt(id)
        .selected_text(shown)
        .width(240.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(chosen, None, "System default");
            for name in names {
                ui.selectable_value(chosen, Some(name.clone()), name);
            }
        });
}
