//! The audio settings calls use (`call.setAudio`), the device list (`call.listDevices`) and
//! the audio test (`call.testAudio`): the microphone played back through the whole call
//! path, to hear the settings without calling anyone.

use std::{
    sync::{Arc, Weak},
    time::Duration,
};

use deskbeam_call::{
    devices::{Devices, Speakers, device_names},
    media::{Media, NoiseLevel, Options, Processing},
};
use deskbeam_model::{AudioDevices, AudioSettings, NoiseSuppression};
use serde_json::Value;

use crate::{
    command::CommandError,
    engine::{Engine, PREFERENCES_KEY},
};

/// The longest an audio test runs.
const TEST_FOR: Duration = Duration::from_secs(15);
/// How often a running test's level is shown.
const TEST_LEVEL_EVERY: Duration = Duration::from_millis(250);

/// The audio of the call or test running now, so new settings reach it.
#[derive(Default)]
pub(crate) struct LiveAudio {
    pub(crate) call: Option<Weak<Media>>,
    pub(crate) test: Option<Arc<Media>>,
    pub(crate) devices: Option<AudioDevices>,
}

/// The call crate's processing for these settings.
pub(crate) fn processing(settings: &AudioSettings) -> Processing {
    Processing {
        echo_cancellation: settings.echo_cancellation,
        noise_suppression: match settings.noise_suppression {
            NoiseSuppression::Off => None,
            NoiseSuppression::Low => Some(NoiseLevel::Low),
            NoiseSuppression::Moderate => Some(NoiseLevel::Moderate),
            NoiseSuppression::High => Some(NoiseLevel::High),
            NoiseSuppression::VeryHigh => Some(NoiseLevel::VeryHigh),
        },
        auto_gain: settings.auto_gain,
        max_gain_db: settings.max_gain_db as f32,
        boost_db: settings.boost_db as f32,
    }
}

impl Engine {
    /// The devices to open: the ones given in the config (tests), or the system's, by the
    /// names chosen in the settings.
    pub(crate) fn audio_devices_for_call(&self) -> Arc<dyn Devices> {
        match &self.inner.call_audio.0 {
            Some(devices) => devices.clone(),
            None => {
                let settings = self.audio_settings();
                Arc::new(Speakers {
                    input: settings.microphone,
                    output: settings.speaker,
                })
            }
        }
    }

    pub(crate) fn audio_settings(&self) -> AudioSettings {
        self.inner.preferences.lock().expect("lock").audio.clone()
    }

    pub(crate) fn set_audio(&self, mut settings: AudioSettings) -> Result<Value, CommandError> {
        settings.max_gain_db = settings.max_gain_db.min(AudioSettings::MOST_GAIN_DB);
        settings.boost_db = settings.boost_db.min(AudioSettings::MOST_BOOST_DB);
        for name in [
            &mut settings.microphone,
            &mut settings.speaker,
            &mut settings.camera,
        ] {
            if name.as_deref().is_some_and(|n| n.trim().is_empty()) {
                *name = None;
            }
        }
        let mut preferences = self.inner.preferences.lock().expect("lock").clone();
        preferences.audio = settings.clone();
        let json = serde_json::to_string(&preferences).map_err(anyhow::Error::from)?;
        self.inner.store.set_setting(PREFERENCES_KEY, &json)?;
        *self.inner.preferences.lock().expect("lock") = preferences;

        let processing = processing(&settings);
        let live = self.inner.audio.lock().expect("lock");
        if let Some(media) = live.call.as_ref().and_then(Weak::upgrade) {
            media.set_processing(processing);
        }
        if let Some(media) = &live.test {
            media.set_processing(processing);
        }
        Ok(Value::Null)
    }

    pub(crate) async fn list_audio_devices(&self) -> Result<Value, CommandError> {
        let ((microphones, speakers), cameras) = tokio::task::spawn_blocking(|| {
            (device_names(), deskbeam_call::video::camera::camera_names())
        })
        .await
        .map_err(|e| CommandError::Failed(e.to_string()))?;
        let devices = AudioDevices {
            microphones,
            speakers,
            cameras,
        };
        self.inner.audio.lock().expect("lock").devices = Some(devices.clone());
        Ok(serde_json::to_value(devices).map_err(anyhow::Error::from)?)
    }

    pub(crate) async fn test_audio(&self, on: bool) -> Result<Value, CommandError> {
        if !on {
            self.stop_audio_test();
            return Ok(Value::Null);
        }
        if self.call_view().is_some_and(|c| c.is_live()) {
            return Err(CommandError::Failed(
                "Not during a call: the call is using the microphone".into(),
            ));
        }
        if self.inner.audio.lock().expect("lock").test.is_some() {
            return Ok(Value::Null);
        }
        let devices = self.audio_devices_for_call();
        let options = Options {
            processing: Some(processing(&self.audio_settings())),
            record: None,
        };
        let media = tokio::task::spawn_blocking(move || Media::loopback(devices.as_ref(), options))
            .await
            .map_err(|e| CommandError::Failed(e.to_string()))?
            .map_err(|e| {
                CommandError::Failed(format!("The microphone or speaker did not open: {e:#}"))
            })?;
        let weak = Arc::downgrade(&media);
        self.inner.audio.lock().expect("lock").test = Some(media);

        // Shows the level while it runs, and stops it after a while.
        let engine = self.clone();
        tokio::spawn(async move {
            let end = tokio::time::Instant::now() + TEST_FOR;
            let mut tick = tokio::time::interval(TEST_LEVEL_EVERY);
            loop {
                tick.tick().await;
                let same = engine
                    .inner
                    .audio
                    .lock()
                    .expect("lock")
                    .test
                    .as_ref()
                    .is_some_and(|t| Arc::downgrade(t).ptr_eq(&weak));
                if !same {
                    return;
                }
                if tokio::time::Instant::now() >= end {
                    engine.stop_audio_test();
                    engine.refresh().await;
                    return;
                }
                engine.refresh().await;
            }
        });
        Ok(Value::Null)
    }

    /// Ends the audio test, if one runs. Its devices close on a blocking thread.
    pub(crate) fn stop_audio_test(&self) {
        let test = self.inner.audio.lock().expect("lock").test.take();
        if let Some(media) = test {
            tokio::task::spawn_blocking(move || drop(media));
        }
    }

    /// The running test's level, for the snapshot.
    pub(crate) fn audio_test_level(&self) -> Option<f32> {
        self.inner
            .audio
            .lock()
            .expect("lock")
            .test
            .as_ref()
            .map(|m| m.stats().level)
    }

    pub(crate) fn audio_device_list(&self) -> Option<AudioDevices> {
        self.inner.audio.lock().expect("lock").devices.clone()
    }
}
