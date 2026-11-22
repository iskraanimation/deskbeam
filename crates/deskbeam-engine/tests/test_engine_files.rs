//! File transfers between two paired engines over n0's public relays: an offer accepted,
//! an offer declined, and a device whose files are accepted without asking.

use std::{path::Path, sync::Arc, time::Duration};

use deskbeam_engine::{
    Command, Engine, EngineConfig, Event,
    model::{Direction, Permission, Snapshot, Transfer, TransferState},
};
use deskbeam_store::SqliteStore;

const START_TIMEOUT: Duration = Duration::from_secs(60);
const JOIN_DEADLINE: Duration = Duration::from_secs(90);
const DEADLINE: Duration = Duration::from_secs(90);

async fn start(dir: &Path, downloads: &Path) -> Engine {
    let mut config = EngineConfig::new(dir);
    config.downloads_dir = Some(downloads.to_path_buf());
    tokio::time::timeout(
        START_TIMEOUT,
        Engine::start_and_wait(config, Arc::new(SqliteStore::open_in_memory().unwrap())),
    )
    .await
    .expect("engine start timed out")
    .expect("engine start failed")
}

async fn wait_for(engine: &Engine, what: &str, check: impl Fn(&Snapshot) -> bool) {
    let mut rx = engine.watch();
    let waited = tokio::time::timeout(DEADLINE, async {
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
        .expect("this device")
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
    wait_for(host, "the host to see the joiner online", |s| {
        s.device(&joiner_id).is_some_and(|d| d.online)
    })
    .await;
    wait_for(joiner, "the joiner to see the host online", |s| {
        s.device(&host_id).is_some_and(|d| d.online)
    })
    .await;
}

fn transfer(snap: &Snapshot, id: &str) -> Option<Transfer> {
    snap.transfers.iter().find(|t| t.id == id).cloned()
}

fn incoming(snap: &Snapshot) -> Option<Transfer> {
    snap.transfers
        .iter()
        .find(|t| t.direction == Direction::Receive && t.state == TransferState::Incoming)
        .cloned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_offered_accepted_declined_and_auto_accepted() {
    let dirs: Vec<_> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let (sender, receiver) = tokio::join!(
        start(dirs[0].path(), dirs[1].path()),
        start(dirs[2].path(), dirs[3].path())
    );
    pair(&sender, &receiver).await;
    let (sender_id, receiver_id) = (my_id(&sender), my_id(&receiver));

    // A file, and a folder with two files in it.
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("report.txt");
    let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&file, &payload).unwrap();
    let folder = source.path().join("photos");
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("a.jpg"), b"aaa").unwrap();
    std::fs::write(folder.join("b.jpg"), b"bbbb").unwrap();

    // The receiver names the sender on its side only; the sender keeps its own name.
    let own = receiver
        .snapshot()
        .device(&sender_id)
        .unwrap()
        .display_name
        .clone();
    receiver
        .execute(Command::SetDeviceName {
            endpoint_id: sender_id.clone(),
            name: "Den's Mac".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        receiver.snapshot().device(&sender_id).unwrap().display_name,
        "Den's Mac"
    );
    let mine = sender
        .snapshot()
        .this_device
        .as_ref()
        .unwrap()
        .display_name
        .clone();
    assert_eq!(mine, own);
    // Received files go in a folder named after the sender.
    let inbox = dirs[3].path().join("Den's Mac");

    // 1. Offered and accepted.
    let mut receiver_events = receiver.events();
    let sent = sender
        .execute(Command::SendFiles {
            endpoint_id: receiver_id.clone(),
            paths: vec![file.display().to_string(), folder.display().to_string()],
        })
        .await
        .unwrap();
    let sent_id = sent["transferId"].as_str().unwrap().to_string();
    let offer = transfer(&sender.snapshot(), &sent_id).unwrap();
    assert_eq!(offer.state, TransferState::Offered);
    assert_eq!(offer.file_count, 3);
    assert_eq!(offer.names, ["report.txt", "photos"]);

    wait_for(&receiver, "the offer to arrive", |s| incoming(s).is_some()).await;
    let offered = incoming(&receiver.snapshot()).unwrap();
    assert_eq!(offered.peer, sender_id);
    assert_eq!(offered.peer_name, "Den's Mac");
    assert_eq!(offered.file_count, 3);
    let heard = tokio::time::timeout(DEADLINE, async {
        loop {
            if let Event::FilesOffered { transfer_id, .. } = receiver_events.recv().await.unwrap() {
                return transfer_id;
            }
        }
    })
    .await
    .expect("the receiver to hear of the offer");
    assert_eq!(heard, offered.id);

    receiver
        .execute(Command::AcceptFiles {
            transfer_id: offered.id.clone(),
        })
        .await
        .unwrap();
    wait_for(&receiver, "the download to finish", |s| {
        transfer(s, &offered.id).is_some_and(|t| t.state.is_finished())
    })
    .await;
    let done = transfer(&receiver.snapshot(), &offered.id).unwrap();
    assert!(
        matches!(done.state, TransferState::Done { .. }),
        "{:?}",
        done.state
    );
    assert_eq!(std::fs::read(inbox.join("report.txt")).unwrap(), payload);
    assert_eq!(
        std::fs::read(inbox.join("photos").join("b.jpg")).unwrap(),
        b"bbbb"
    );
    wait_for(&sender, "the sender to see it done", |s| {
        transfer(s, &sent_id).is_some_and(|t| matches!(t.state, TransferState::Done { .. }))
    })
    .await;

    // Answering twice is refused.
    assert!(
        receiver
            .execute(Command::AcceptFiles {
                transfer_id: offered.id.clone(),
            })
            .await
            .is_err()
    );

    // 2. Declined.
    let sent = sender
        .execute(Command::SendFiles {
            endpoint_id: receiver_id.clone(),
            paths: vec![file.display().to_string()],
        })
        .await
        .unwrap();
    let declined_id = sent["transferId"].as_str().unwrap().to_string();
    wait_for(&receiver, "the second offer", |s| incoming(s).is_some()).await;
    let offered = incoming(&receiver.snapshot()).unwrap();
    receiver
        .execute(Command::DeclineFiles {
            transfer_id: offered.id,
        })
        .await
        .unwrap();
    wait_for(&sender, "the sender to see the decline", |s| {
        transfer(s, &declined_id).is_some_and(|t| t.state == TransferState::Declined)
    })
    .await;

    // 3. Accepted without asking.
    receiver
        .execute(Command::SetPermission {
            endpoint_id: sender_id.clone(),
            permission: Permission::AutoAcceptFiles,
            allowed: true,
        })
        .await
        .unwrap();
    std::fs::write(source.path().join("quick.txt"), b"no questions").unwrap();
    sender
        .execute(Command::SendFiles {
            endpoint_id: receiver_id.clone(),
            paths: vec![source.path().join("quick.txt").display().to_string()],
        })
        .await
        .unwrap();
    let landed = tokio::time::timeout(DEADLINE, async {
        loop {
            if let Ok(bytes) = std::fs::read(inbox.join("quick.txt")) {
                return bytes;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    })
    .await
    .expect("the file to land without asking");
    assert_eq!(landed, b"no questions");

    // Finished transfers can be cleared away.
    receiver.execute(Command::ClearTransfers).await.unwrap();
    assert!(
        receiver
            .snapshot()
            .transfers
            .iter()
            .all(|t| !t.state.is_finished())
    );

    // An empty name goes back to the device's own.
    receiver
        .execute(Command::SetDeviceName {
            endpoint_id: sender_id.clone(),
            name: " ".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        receiver.snapshot().device(&sender_id).unwrap().display_name,
        own
    );

    sender.shutdown().await;
    receiver.shutdown().await;
}
