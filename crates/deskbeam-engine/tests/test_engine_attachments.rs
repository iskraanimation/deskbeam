//! Files attached to chat messages, through engines over n0's public relays: a picture comes
//! with its preview and is fetched on its own, another file only when asked for; an unpaired
//! device can't fetch; and in a group a member gets a file from another member after the
//! sender has gone offline.

use std::{path::Path, sync::Arc, time::Duration};

use deskbeam_engine::{
    Command, Engine, EngineConfig,
    model::{AttachmentState, ChatAttachment, Snapshot},
};
use deskbeam_store::SqliteStore;

const START_TIMEOUT: Duration = Duration::from_secs(60);
const JOIN_DEADLINE: Duration = Duration::from_secs(90);
const ARRIVE_DEADLINE: Duration = Duration::from_secs(150);

async fn start(dir: &Path) -> Engine {
    let store = Arc::new(SqliteStore::open_in(dir).unwrap());
    let mut config = EngineConfig::new(dir);
    config.downloads_dir = Some(dir.join("Downloads"));
    tokio::time::timeout(START_TIMEOUT, Engine::start_and_wait(config, store))
        .await
        .expect("engine start timed out")
        .expect("engine start failed")
}

async fn wait_for(engine: &Engine, what: &str, check: impl Fn(&Snapshot) -> bool) {
    let mut rx = engine.watch();
    let waited = tokio::time::timeout(ARRIVE_DEADLINE, async {
        loop {
            if check(&rx.borrow_and_update()) {
                return;
            }
            rx.changed().await.expect("engine gone");
        }
    })
    .await;
    assert!(waited.is_ok(), "timed out waiting for {what}");
}

fn my_id(engine: &Engine) -> String {
    engine
        .snapshot()
        .this_device
        .as_ref()
        .unwrap()
        .endpoint_id
        .clone()
}

async fn pair(host: &Engine, joiner: &Engine) {
    let opened = host
        .execute(Command::OpenPairing {
            ttl_secs: Some(300),
        })
        .await
        .unwrap();
    let code = opened["code"].as_str().unwrap().to_string();
    let end = tokio::time::Instant::now() + JOIN_DEADLINE;
    loop {
        match tokio::time::timeout(
            Duration::from_secs(30),
            joiner.execute(Command::JoinPairing { code: code.clone() }),
        )
        .await
        {
            Ok(Ok(_)) => break,
            Ok(Err(e)) => eprintln!("join failed, retrying: {e}"),
            Err(_) => eprintln!("join timed out, retrying"),
        }
        assert!(tokio::time::Instant::now() < end, "pairing did not succeed");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    host.execute(Command::ClosePairing).await.unwrap();
    let (host_id, joiner_id) = (my_id(host), my_id(joiner));
    wait_for(host, "the host to list the joiner online", |s| {
        s.device(&joiner_id).is_some_and(|d| d.online)
    })
    .await;
    wait_for(joiner, "the joiner to list the host online", |s| {
        s.device(&host_id).is_some_and(|d| d.online)
    })
    .await;
}

/// The attachments of the message with `body` in `conversation`, once it is there.
async fn attachments_of(
    engine: &Engine,
    conversation: &str,
    body: &str,
) -> (String, Vec<ChatAttachment>) {
    let found = tokio::time::timeout(ARRIVE_DEADLINE, async {
        loop {
            if let Some(m) = engine
                .chat_messages(conversation, 50)
                .into_iter()
                .find(|m| m.body == body)
            {
                return (m.id, m.attachments);
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    })
    .await;
    found.expect("the message to arrive")
}

/// Waits until the attachment `hash` of `conversation`'s message `body` is in `state`.
async fn wait_state(engine: &Engine, conversation: &str, body: &str, hash: &str, ready: bool) {
    let waited = tokio::time::timeout(ARRIVE_DEADLINE, async {
        loop {
            let (_, list) = attachments_of(engine, conversation, body).await;
            let state = list
                .iter()
                .find(|a| a.hash == hash)
                .map(|a| a.state.clone());
            if (state == Some(AttachmentState::Ready)) == ready {
                return;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    })
    .await;
    assert!(
        waited.is_ok(),
        "timed out waiting for {hash} to be ready={ready}"
    );
}

fn write_picture(path: &Path) {
    image::RgbImage::from_fn(800, 600, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 90])
    })
    .save(path)
    .unwrap();
}

fn write_bytes(path: &Path, len: usize) -> Vec<u8> {
    let bytes: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
    std::fs::write(path, &bytes).unwrap();
    bytes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pictures_come_on_their_own_and_files_when_asked() {
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, b) = tokio::join!(start(dirs[0].path()), start(dirs[1].path()));
    pair(&a, &b).await;
    let b_id = my_id(&b);
    let conversation = a.direct_conversation(&b_id).unwrap();

    let files = tempfile::tempdir().unwrap();
    let picture = files.path().join("view.png");
    write_picture(&picture);
    let report = files.path().join("report.bin");
    let bytes = write_bytes(&report, 3 * 1024 * 1024 + 17);
    a.execute(Command::SendMessage {
        conversation: conversation.clone(),
        body: "two files".into(),
        attachments: vec![picture.display().to_string(), report.display().to_string()],
        reply_to: None,
    })
    .await
    .unwrap();
    // The sender has both at once; moving the originals doesn't matter.
    std::fs::remove_file(&report).unwrap();
    let (_, sent) = attachments_of(&a, &conversation, "two files").await;
    assert!(sent.iter().all(|x| x.state == AttachmentState::Ready));

    let (id, got) = attachments_of(&b, &conversation, "two files").await;
    let (pic, file) = (&got[0], &got[1]);
    assert_eq!(
        (pic.name.as_str(), pic.mime.as_str()),
        ("view.png", "image/png")
    );
    assert_eq!((pic.width, pic.height), (Some(800), Some(600)));
    assert!(pic.preview.is_some(), "a picture brings its preview");
    assert!(deskbeam_engine::preview_picture(pic.preview.as_ref().unwrap()).is_some());
    assert_eq!(
        (file.name.as_str(), file.size),
        ("report.bin", bytes.len() as u64)
    );
    assert!(file.preview.is_none());

    // The picture comes on its own; the other file waits to be asked for.
    wait_state(&b, &conversation, "two files", &pic.hash, true).await;
    assert!(
        b.attachment_picture(&pic.hash, 256)
            .await
            .is_some_and(|p| p.width == 256)
    );
    // At full size it comes at its own pixels, never scaled up.
    assert!(
        b.attachment_picture(&pic.hash, 8192)
            .await
            .is_some_and(|p| (p.width, p.height) == (800, 600))
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (_, now) = attachments_of(&b, &conversation, "two files").await;
    assert_eq!(now[1].state, AttachmentState::Missing);

    b.execute(Command::FetchAttachment {
        message_id: id.clone(),
        hash: file.hash.clone(),
    })
    .await
    .unwrap();
    let saved = b
        .execute(Command::SaveAttachment {
            message_id: id.clone(),
            hash: file.hash.clone(),
            path: None,
        })
        .await
        .unwrap();
    let path = std::path::PathBuf::from(saved["path"].as_str().unwrap());
    assert!(
        path.starts_with(dirs[1].path().join("Downloads")),
        "{path:?}"
    );
    assert_eq!(path.file_name().unwrap(), "report.bin");
    assert_eq!(std::fs::read(&path).unwrap(), bytes);

    // A device that is not paired is refused, even knowing the hash.
    let stranger = iroh::Endpoint::bind(iroh::endpoint::presets::N0)
        .await
        .unwrap();
    let store = iroh_blobs::store::mem::MemStore::new();
    let a_id: iroh::EndpointId = my_id(&a).parse().unwrap();
    let hash: iroh_blobs::Hash = file.hash.parse().unwrap();
    let connection = tokio::time::timeout(
        Duration::from_secs(60),
        stranger.connect(a_id, iroh_blobs::ALPN),
    )
    .await
    .expect("connecting timed out")
    .expect("the stranger reaches a; refusing is the blobs protocol's job");
    let fetched = tokio::time::timeout(
        Duration::from_secs(30),
        store.remote().fetch(connection, hash).complete(),
    )
    .await
    .expect("the refusal came");
    assert!(fetched.is_err(), "an unpaired device fetched an attachment");
    assert!(!store.blobs().has(hash).await.unwrap());

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_gets_a_file_from_another_after_the_sender_left() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, b, c) = tokio::join!(
        start(dirs[0].path()),
        start(dirs[1].path()),
        start(dirs[2].path())
    );
    pair(&a, &b).await;
    pair(&a, &c).await;
    pair(&b, &c).await;
    let made = a
        .execute(Command::CreateGroup {
            name: "Team".into(),
            members: vec![my_id(&b), my_id(&c)],
        })
        .await
        .unwrap();
    let group = made["conversation"].as_str().unwrap().to_string();
    for (engine, who) in [(&b, "b"), (&c, "c")] {
        wait_for(engine, &format!("{who} to get the group"), |s| {
            s.conversation(&group).is_some()
        })
        .await;
    }

    let files = tempfile::tempdir().unwrap();
    let notes = files.path().join("notes.txt");
    let bytes = write_bytes(&notes, 600 * 1024);
    b.execute(Command::SendMessage {
        conversation: group.clone(),
        body: String::new(),
        attachments: vec![notes.display().to_string()],
        reply_to: None,
    })
    .await
    .unwrap();

    // a fetches it from b; then b goes away, and c gets it from a.
    let (id, list) = attachments_of(&a, &group, "").await;
    let hash = list[0].hash.clone();
    a.execute(Command::FetchAttachment {
        message_id: id.clone(),
        hash: hash.clone(),
    })
    .await
    .unwrap();
    let (c_id, _) = attachments_of(&c, &group, "").await;
    assert_eq!(c_id, id);
    b.shutdown().await;
    drop(b);
    wait_for(&c, "c to see b offline", |s| {
        s.devices.iter().filter(|d| d.online).count() == 1
    })
    .await;
    c.execute(Command::FetchAttachment {
        message_id: id.clone(),
        hash: hash.clone(),
    })
    .await
    .unwrap();
    let saved = c
        .execute(Command::SaveAttachment {
            message_id: id,
            hash,
            path: Some(files.path().join("copy.txt").display().to_string()),
        })
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(saved["path"].as_str().unwrap()).unwrap(),
        bytes
    );

    a.shutdown().await;
    c.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn freeing_space_removes_received_files_only() {
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, b) = tokio::join!(start(dirs[0].path()), start(dirs[1].path()));
    pair(&a, &b).await;
    let conversation = a.direct_conversation(&my_id(&b)).unwrap();

    let files = tempfile::tempdir().unwrap();
    let big = files.path().join("big.bin");
    write_bytes(&big, 2 * 1024 * 1024 + 5);
    a.execute(Command::SendMessage {
        conversation: conversation.clone(),
        body: "a big file".into(),
        attachments: vec![big.display().to_string()],
        reply_to: None,
    })
    .await
    .unwrap();
    let (id, got) = attachments_of(&b, &conversation, "a big file").await;
    let hash = got[0].hash.clone();
    b.execute(Command::FetchAttachment {
        message_id: id.clone(),
        hash: hash.clone(),
    })
    .await
    .unwrap();
    wait_state(&b, &conversation, "a big file", &hash, true).await;
    let before = b.attachments_size();
    assert!(before > 2 * 1024 * 1024, "{before}");

    // b frees space: the file it received goes, the message stays.
    let freed = b.execute(Command::FreeAttachmentSpace).await.unwrap();
    assert_eq!(freed["files"], 1, "{freed}");
    assert!(freed["freed"].as_u64().unwrap() > 1024 * 1024, "{freed}");
    let (_, now) = attachments_of(&b, &conversation, "a big file").await;
    assert_eq!(now[0].state, AttachmentState::Missing);
    assert!(b.attachments_size() < before - 1024 * 1024);

    // a sent it: freeing space on a keeps it, so b can fetch it again.
    let kept = a.execute(Command::FreeAttachmentSpace).await.unwrap();
    assert_eq!(kept["files"], 0, "{kept}");
    b.execute(Command::FetchAttachment {
        message_id: id,
        hash: hash.clone(),
    })
    .await
    .unwrap();
    wait_state(&b, &conversation, "a big file", &hash, true).await;

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_video_is_fetched_and_copied_out_to_play() {
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, b) = tokio::join!(start(dirs[0].path()), start(dirs[1].path()));
    pair(&a, &b).await;
    let conversation = a.direct_conversation(&my_id(&b)).unwrap();
    let files = tempfile::tempdir().unwrap();
    let clip = files.path().join("clip.mp4");
    let bytes = write_bytes(&clip, 300_000);
    a.execute(Command::SendMessage {
        conversation: conversation.clone(),
        body: String::new(),
        attachments: vec![clip.display().to_string()],
        reply_to: None,
    })
    .await
    .unwrap();
    let (id, got) = attachments_of(&b, &conversation, "").await;
    assert_eq!(got[0].mime, "video/mp4");
    assert!(got[0].plays());

    // Not here yet: opening fetches it, then hands a copy to play.
    let opened = b
        .execute(Command::OpenAttachment {
            message_id: id.clone(),
            hash: got[0].hash.clone(),
        })
        .await
        .unwrap();
    let path = std::path::PathBuf::from(opened["path"].as_str().unwrap());
    assert!(path.starts_with(dirs[1].path().join("opened")), "{path:?}");
    assert_eq!(path.file_name().unwrap(), "clip.mp4");
    assert_eq!(std::fs::read(&path).unwrap(), bytes);

    // Freeing space removes the copy too.
    b.execute(Command::FreeAttachmentSpace).await.unwrap();
    assert!(!path.exists());

    a.shutdown().await;
    b.shutdown().await;
}
