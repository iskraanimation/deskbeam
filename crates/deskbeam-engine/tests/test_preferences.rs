//! Preferences through `preferences.set`: what is kept, and that it survives a restart.
//! Needs no network: preferences are handled before the node is up.

use std::sync::Arc;

use deskbeam_engine::{Command, Engine, EngineConfig, model::RECENT_EMOJI};
use deskbeam_store::SqliteStore;

fn start(dir: &std::path::Path) -> Engine {
    Engine::start(
        EngineConfig::new(dir),
        Arc::new(SqliteStore::open_in(dir).unwrap()),
    )
}

fn recent(list: Vec<String>) -> Command {
    Command::SetPreferences {
        close_to_tray: None,
        notifications: None,
        window: None,
        recent_emoji: Some(list),
        auto_fetch_images: None,
        message_sound: None,
        settings_window: None,
    }
}

#[tokio::test]
async fn recent_emoji_are_kept_once_in_order_and_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let engine = start(dir.path());
    assert!(engine.preferences().recent_emoji.is_empty());

    let mut list: Vec<String> = ["😀", " 🎉 ", "😀", "", &"x".repeat(40)]
        .iter()
        .map(|s| s.to_string())
        .collect();
    list.extend((0..40).map(|i| format!("e{i}")));
    engine.execute(recent(list)).await.unwrap();

    let kept = engine.preferences().recent_emoji;
    assert_eq!(kept.len(), RECENT_EMOJI);
    assert_eq!(&kept[..3], ["😀", "🎉", "e0"], "trimmed, once, in order");
    assert!(
        engine.preferences().close_to_tray,
        "other preferences untouched"
    );

    engine.shutdown().await;
    drop(engine);
    let engine = start(dir.path());
    assert_eq!(engine.preferences().recent_emoji, kept);
    engine.shutdown().await;
}

#[tokio::test]
async fn chosen_sounds_are_copied_in_and_can_be_reset() {
    use deskbeam_engine::model::SoundKind;
    let dir = tempfile::tempdir().unwrap();
    let engine = start(dir.path());
    assert!(engine.sound_file(SoundKind::Ring).is_none());

    let source = tempfile::tempdir().unwrap();
    let chosen = source.path().join("Old Phone.MP3");
    std::fs::write(&chosen, b"not really an mp3").unwrap();
    engine
        .execute(Command::SetSound {
            kind: SoundKind::Ring,
            path: Some(chosen.display().to_string()),
        })
        .await
        .unwrap();
    std::fs::remove_file(&chosen).unwrap();
    let copied = engine.sound_file(SoundKind::Ring).expect("copied in");
    assert!(copied.starts_with(dir.path()));
    assert_eq!(copied.file_name().unwrap(), "ring.mp3");
    assert_eq!(
        engine.preferences().ring_tone.unwrap().name,
        "Old Phone.MP3"
    );
    assert!(engine.sound_file(SoundKind::Message).is_none());

    // Only sound files, and back to the built-in one.
    let text = source.path().join("notes.txt");
    std::fs::write(&text, b"x").unwrap();
    assert!(
        engine
            .execute(Command::SetSound {
                kind: SoundKind::Message,
                path: Some(text.display().to_string()),
            })
            .await
            .is_err()
    );
    engine
        .execute(Command::SetSound {
            kind: SoundKind::Ring,
            path: None,
        })
        .await
        .unwrap();
    assert!(engine.sound_file(SoundKind::Ring).is_none());
    assert!(!copied.exists(), "the copy is removed");
    engine.shutdown().await;
}

#[tokio::test]
async fn audio_settings_are_kept_within_bounds_and_survive_a_restart() {
    use deskbeam_engine::model::{AudioSettings, CameraQuality, NoiseSuppression};
    let dir = tempfile::tempdir().unwrap();
    let engine = start(dir.path());
    assert_eq!(engine.preferences().audio, AudioSettings::default());

    let asked = AudioSettings {
        microphone: Some("USB Mic".into()),
        speaker: Some("  ".into()),
        camera: Some("FaceTime HD Camera".into()),
        camera_quality: CameraQuality::Hd,
        echo_cancellation: false,
        noise_suppression: NoiseSuppression::High,
        auto_gain: true,
        max_gain_db: 200,
        boost_db: 200,
    };
    engine
        .execute(Command::SetAudio { settings: asked })
        .await
        .unwrap();
    let kept = engine.preferences().audio;
    assert_eq!(kept.microphone.as_deref(), Some("USB Mic"));
    assert_eq!(kept.speaker, None, "a blank name is the default device");
    assert_eq!(kept.max_gain_db, AudioSettings::MOST_GAIN_DB);
    assert_eq!(kept.boost_db, AudioSettings::MOST_BOOST_DB);
    assert_eq!(kept.noise_suppression, NoiseSuppression::High);
    assert_eq!(kept.camera_quality, CameraQuality::Hd);
    assert_eq!(engine.snapshot().preferences.audio, kept);

    engine.shutdown().await;
    drop(engine);
    let engine = start(dir.path());
    assert_eq!(engine.preferences().audio, kept);
    engine.shutdown().await;
}

#[tokio::test]
async fn the_audio_test_plays_the_microphone_back_and_stops() {
    use deskbeam_engine::{CallAudio, calls::devices::ToneDevices};
    let dir = tempfile::tempdir().unwrap();
    let devices = ToneDevices {
        tone: Some(440.0),
        ..Default::default()
    };
    let mut config = EngineConfig::new(dir.path());
    config.audio = CallAudio(Some(Arc::new(devices.clone())));
    let engine = Engine::start(config, Arc::new(SqliteStore::open_in(dir.path()).unwrap()));

    engine
        .execute(Command::TestAudio { on: true })
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    let level = engine.snapshot().audio_test;
    assert!(level.is_some_and(|l| l > 0.0), "{level:?}");
    // What the "microphone" said came back to the "speaker".
    let heard = devices.heard.lock().unwrap().clone();
    let loudest = heard.iter().fold(0.0f32, |a, s| a.max(s.abs()));
    assert!(loudest > 0.01, "heard {loudest}");

    engine
        .execute(Command::TestAudio { on: false })
        .await
        .unwrap();
    assert_eq!(engine.snapshot().audio_test, None);
    engine.shutdown().await;
}

#[tokio::test]
async fn the_settings_window_place_is_kept_apart_from_the_main_one() {
    use deskbeam_engine::model::WindowPlace;
    let dir = tempfile::tempdir().unwrap();
    let engine = start(dir.path());
    let place = WindowPlace {
        x: Some(300),
        y: Some(120),
        width: 610,
        height: 700,
        maximized: false,
        monitor: Some((1440, 900)),
    };
    engine
        .execute(Command::SetPreferences {
            close_to_tray: None,
            notifications: None,
            window: None,
            recent_emoji: None,
            auto_fetch_images: None,
            message_sound: None,
            settings_window: Some(place),
        })
        .await
        .unwrap();
    assert_eq!(engine.preferences().settings_window, Some(place));
    assert_eq!(
        engine.preferences().window,
        None,
        "the main window's untouched"
    );

    engine.shutdown().await;
    drop(engine);
    let engine = start(dir.path());
    assert_eq!(engine.preferences().settings_window, Some(place));
    engine.shutdown().await;
}
