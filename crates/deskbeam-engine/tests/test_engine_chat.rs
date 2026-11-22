//! Messaging through engines over n0's public relays: a message waits for a device that is
//! off and arrives when it is back; group members catch up through a member that was online;
//! and a group member a device is not paired with is left out there.

use std::{path::Path, sync::Arc, time::Duration};

use deskbeam_engine::{
    Command, Engine, EngineConfig,
    model::{ConversationKind, Snapshot},
};
use deskbeam_store::SqliteStore;

const START_TIMEOUT: Duration = Duration::from_secs(60);
const JOIN_DEADLINE: Duration = Duration::from_secs(90);
/// Long enough for a restarted device to be seen again: the presence connection retries
/// for up to a minute.
const ARRIVE_DEADLINE: Duration = Duration::from_secs(150);

/// An engine on `dir` with its database there too, so a restart keeps everything.
async fn start(dir: &Path) -> Engine {
    let store = Arc::new(SqliteStore::open_in(dir).unwrap());
    tokio::time::timeout(
        START_TIMEOUT,
        Engine::start_and_wait(EngineConfig::new(dir), store),
    )
    .await
    .expect("engine start timed out")
    .expect("engine start failed")
}

async fn wait_for(
    engine: &Engine,
    what: &str,
    deadline: Duration,
    check: impl Fn(&Snapshot) -> bool,
) {
    let mut rx = engine.watch();
    let waited = tokio::time::timeout(deadline, async {
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
    wait_for(host, "the host to list the joiner", JOIN_DEADLINE, |s| {
        s.device(&joiner_id).is_some()
    })
    .await;
    wait_for(joiner, "the joiner to list the host", JOIN_DEADLINE, |s| {
        s.device(&host_id).is_some()
    })
    .await;
}

/// Waits until `engine` holds a message with `body` in `conversation`.
async fn wait_for_message(engine: &Engine, conversation: &str, body: &str, what: &str) {
    let waited = tokio::time::timeout(ARRIVE_DEADLINE, async {
        loop {
            if engine
                .chat_messages(conversation, 100)
                .iter()
                .any(|m| m.body == body)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_waits_for_a_device_that_is_off() {
    let a_dir = tempfile::tempdir().unwrap();
    let b_dir = tempfile::tempdir().unwrap();
    let (a, b) = tokio::join!(start(a_dir.path()), start(b_dir.path()));
    pair(&a, &b).await;
    let (a_id, b_id) = (my_id(&a), my_id(&b));
    let conversation = a.direct_conversation(&b_id).unwrap();
    assert_eq!(Some(conversation.clone()), b.direct_conversation(&a_id));

    // While both are on, a message goes straight through.
    a.execute(Command::SendMessage {
        conversation: conversation.clone(),
        body: "hello".into(),
        attachments: Vec::new(),
        reply_to: None,
    })
    .await
    .unwrap();
    wait_for_message(&b, &conversation, "hello", "the first message").await;
    let unread = b.snapshot().conversation(&conversation).map(|c| c.unread);
    assert_eq!(unread, Some(1));
    b.execute(Command::MarkRead {
        conversation: conversation.clone(),
    })
    .await
    .unwrap();
    assert_eq!(b.snapshot().conversation(&conversation).unwrap().unread, 0);

    // b goes off; a writes; the message waits in a's outbox.
    b.shutdown().await;
    drop(b);
    wait_for(&a, "a to see b go offline", ARRIVE_DEADLINE, |s| {
        s.device(&b_id).is_some_and(|d| !d.online)
    })
    .await;
    a.execute(Command::SendMessage {
        conversation: conversation.clone(),
        body: "are you there?".into(),
        attachments: Vec::new(),
        reply_to: None,
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let waiting = a
        .chat_messages(&conversation, 10)
        .into_iter()
        .find(|m| m.body == "are you there?")
        .unwrap();
    assert!(waiting.mine);
    assert_eq!(waiting.delivery.unwrap().waiting, 1, "{waiting:?}");

    // b comes back on and gets it; a sees it delivered.
    let b = start(b_dir.path()).await;
    wait_for_message(&b, &conversation, "are you there?", "the waiting message").await;
    let messages = b.chat_messages(&conversation, 10);
    let bodies: Vec<_> = messages.iter().map(|m| m.body.as_str()).collect();
    assert_eq!(bodies, ["hello", "are you there?"], "in order");
    assert_eq!(messages[1].author, a_id);
    assert!(!messages[1].mine);

    let delivered = tokio::time::timeout(ARRIVE_DEADLINE, async {
        loop {
            let m = a
                .chat_messages(&conversation, 10)
                .into_iter()
                .find(|m| m.body == "are you there?")
                .unwrap();
            if m.delivery
                .is_some_and(|d| d.delivered == 1 && d.waiting == 0)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    })
    .await;
    assert!(delivered.is_ok(), "a to see the message delivered");

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_members_catch_up_through_another_member() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, b, c) = tokio::join!(
        start(dirs[0].path()),
        start(dirs[1].path()),
        start(dirs[2].path())
    );
    pair(&a, &b).await;
    pair(&a, &c).await;
    pair(&b, &c).await;
    let (b_id, c_id) = (my_id(&b), my_id(&c));

    let made = a
        .execute(Command::CreateGroup {
            name: "Team".into(),
            members: vec![b_id.clone(), c_id.clone()],
        })
        .await
        .unwrap();
    let group = made["conversation"].as_str().unwrap().to_string();

    // a hands the group to b and c.
    for (engine, who) in [(&b, "b"), (&c, "c")] {
        wait_for(
            engine,
            &format!("{who} to get the group"),
            ARRIVE_DEADLINE,
            |s| s.conversation(&group).is_some(),
        )
        .await;
    }
    let seen_by_b = b.snapshot();
    let ConversationKind::Group { members, mine } = &seen_by_b.conversation(&group).unwrap().kind
    else {
        panic!("not a group");
    };
    assert!(!mine);
    assert_eq!(members.len(), 2, "b sees a and c");

    b.execute(Command::SendMessage {
        conversation: group.clone(),
        body: "hi team".into(),
        attachments: Vec::new(),
        reply_to: None,
    })
    .await
    .unwrap();
    wait_for_message(&c, &group, "hi team", "c to get b's message").await;
    wait_for_message(&a, &group, "hi team", "a to get b's message").await;

    // c goes off. b writes again, and goes off too. When c is back, the message reaches it
    // from a, which held it.
    c.shutdown().await;
    drop(c);
    b.execute(Command::SendMessage {
        conversation: group.clone(),
        body: "while you were away".into(),
        attachments: Vec::new(),
        reply_to: None,
    })
    .await
    .unwrap();
    wait_for_message(&a, &group, "while you were away", "a to get it").await;
    b.shutdown().await;
    drop(b);

    let c = start(dirs[2].path()).await;
    wait_for_message(&c, &group, "while you were away", "c to catch up through a").await;
    let bodies: Vec<_> = c
        .chat_messages(&group, 10)
        .into_iter()
        .map(|m| m.body)
        .collect();
    assert_eq!(bodies, ["hi team", "while you were away"]);

    a.shutdown().await;
    c.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_members_not_paired_with_each_other_are_left_out() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, b, c) = tokio::join!(
        start(dirs[0].path()),
        start(dirs[1].path()),
        start(dirs[2].path())
    );
    // a is paired with b and with c; b and c are not paired with each other.
    pair(&a, &b).await;
    pair(&a, &c).await;
    let (a_id, b_id, c_id) = (my_id(&a), my_id(&b), my_id(&c));

    let made = a
        .execute(Command::CreateGroup {
            name: "Team".into(),
            members: vec![b_id.clone(), c_id.clone()],
        })
        .await
        .unwrap();
    let group = made["conversation"].as_str().unwrap().to_string();
    for (engine, who) in [(&b, "b"), (&c, "c")] {
        wait_for(
            engine,
            &format!("{who} to get the group"),
            ARRIVE_DEADLINE,
            |s| s.conversation(&group).is_some(),
        )
        .await;
    }

    // Each sees a only.
    for (engine, who) in [(&b, "b"), (&c, "c")] {
        let snap = engine.snapshot();
        let ConversationKind::Group { members, .. } = &snap.conversation(&group).unwrap().kind
        else {
            panic!("not a group");
        };
        let ids: Vec<_> = members.iter().map(|m| m.endpoint_id.as_str()).collect();
        assert_eq!(ids, [a_id.as_str()], "{who} sees only a");
    }

    // b writes: it goes to a only, so nothing waits for c.
    let sent = b
        .execute(Command::SendMessage {
            conversation: group.clone(),
            body: "from b".into(),
            attachments: Vec::new(),
            reply_to: None,
        })
        .await
        .unwrap();
    let id = sent["id"].as_str().unwrap().to_string();
    wait_for_message(&a, &group, "from b", "a to get b's message").await;
    let delivery = b
        .chat_messages(&group, 10)
        .into_iter()
        .find(|m| m.id == id)
        .and_then(|m| m.delivery)
        .expect("b's own message has a delivery count");
    assert_eq!((delivery.delivered, delivery.waiting), (1, 0));

    // a writes too. c restarts, which makes it sync with a straight away: a holds b's
    // message and offers it, and c drops it.
    a.execute(Command::SendMessage {
        conversation: group.clone(),
        body: "from a".into(),
        attachments: Vec::new(),
        reply_to: None,
    })
    .await
    .unwrap();
    wait_for_message(&b, &group, "from a", "b to get a's message").await;
    c.shutdown().await;
    drop(c);
    let c = start(dirs[2].path()).await;
    wait_for_message(&c, &group, "from a", "c to get a's message").await;
    // Its first sync with a runs within a few seconds of starting.
    tokio::time::sleep(Duration::from_secs(10)).await;
    let at_c: Vec<_> = c
        .chat_messages(&group, 10)
        .into_iter()
        .map(|m| m.body)
        .collect();
    assert_eq!(at_c, ["from a"], "c drops b's message: they are not paired");

    a.shutdown().await;
    b.shutdown().await;
    c.shutdown().await;
}

/// Waits until `check` holds for b's view of the conversation.
async fn wait_for_view(
    engine: &Engine,
    conversation: &str,
    what: &str,
    check: impl Fn(&[deskbeam_engine::model::ChatMessage]) -> bool,
) {
    let waited = tokio::time::timeout(ARRIVE_DEADLINE, async {
        loop {
            if check(&engine.chat_messages(conversation, 100)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replies_edits_deletions_and_search() {
    let a_dir = tempfile::tempdir().unwrap();
    let b_dir = tempfile::tempdir().unwrap();
    let (a, b) = tokio::join!(start(a_dir.path()), start(b_dir.path()));
    pair(&a, &b).await;
    let conversation = a.direct_conversation(&my_id(&b)).unwrap();

    let sent = a
        .execute(Command::SendMessage {
            conversation: conversation.clone(),
            body: "lunch at noon?".into(),
            attachments: Vec::new(),
            reply_to: None,
        })
        .await
        .unwrap();
    let first = sent["id"].as_str().unwrap().to_string();
    wait_for_message(&b, &conversation, "lunch at noon?", "the first message").await;

    // b answers it: the reply quotes it, on both sides.
    b.execute(Command::SendMessage {
        conversation: conversation.clone(),
        body: "yes".into(),
        attachments: Vec::new(),
        reply_to: Some(first.clone()),
    })
    .await
    .unwrap();
    wait_for_view(&a, &conversation, "a to get the reply", |list| {
        list.iter().any(|m| {
            m.body == "yes" && m.reply.as_ref().is_some_and(|q| q.text == "lunch at noon?")
        })
    })
    .await;
    b.execute(Command::MarkRead {
        conversation: conversation.clone(),
    })
    .await
    .unwrap();

    // a edits it: b sees the new text, marked, in the quote too, and nothing new to read.
    a.execute(Command::EditMessage {
        message_id: first.clone(),
        body: "lunch at one?".into(),
    })
    .await
    .unwrap();
    wait_for_view(&b, &conversation, "b to see the edit", |list| {
        list.len() == 2
            && list[0].body == "lunch at one?"
            && list[0].edited
            && list[1]
                .reply
                .as_ref()
                .is_some_and(|q| q.text == "lunch at one?")
    })
    .await;
    assert_eq!(b.snapshot().conversation(&conversation).unwrap().unread, 0);
    // Only its author may change it.
    assert!(
        b.execute(Command::EditMessage {
            message_id: first.clone(),
            body: "hijacked".into(),
        })
        .await
        .is_err()
    );

    // Search finds the text as it reads now.
    let hits = b.chat_search("ONE?", None, 10);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].message.id, first);
    assert!(
        b.chat_search("noon", None, 10).is_empty(),
        "the old text is gone"
    );

    // a deletes it: gone for b too, the quote says so, and search no longer finds it.
    a.execute(Command::DeleteMessage {
        message_id: first.clone(),
    })
    .await
    .unwrap();
    wait_for_view(&b, &conversation, "b to see the deletion", |list| {
        list[0].deleted
            && list[0].body.is_empty()
            && list[1].reply.as_ref().is_some_and(|q| q.deleted)
    })
    .await;
    assert!(b.chat_search("one", None, 10).is_empty());
    assert!(
        a.execute(Command::EditMessage {
            message_id: first,
            body: "back".into(),
        })
        .await
        .is_err(),
        "a deleted message stays deleted"
    );

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typing_shows_briefly_and_ends_with_the_message() {
    let a_dir = tempfile::tempdir().unwrap();
    let b_dir = tempfile::tempdir().unwrap();
    let (a, b) = tokio::join!(start(a_dir.path()), start(b_dir.path()));
    pair(&a, &b).await;
    let conversation = a.direct_conversation(&my_id(&b)).unwrap();
    a.execute(Command::SendMessage {
        conversation: conversation.clone(),
        body: "hi".into(),
        attachments: Vec::new(),
        reply_to: None,
    })
    .await
    .unwrap();
    wait_for_message(&b, &conversation, "hi", "the first message").await;
    let a_name = a
        .snapshot()
        .this_device
        .as_ref()
        .unwrap()
        .display_name
        .clone();
    let typing = |s: &deskbeam_engine::Snapshot| {
        s.conversation(&conversation)
            .map(|c| c.typing.clone())
            .unwrap_or_default()
    };

    a.execute(Command::Typing {
        conversation: conversation.clone(),
    })
    .await
    .unwrap();
    wait_for(&b, "b to see a typing", ARRIVE_DEADLINE, |s| {
        typing(s) == [a_name.clone()]
    })
    .await;
    // It fades when a stops.
    wait_for(&b, "a's typing to fade", Duration::from_secs(15), |s| {
        typing(s).is_empty()
    })
    .await;

    // Typing again, then the message ends it at once.
    tokio::time::sleep(Duration::from_secs(3)).await;
    a.execute(Command::Typing {
        conversation: conversation.clone(),
    })
    .await
    .unwrap();
    wait_for(&b, "b to see a typing again", ARRIVE_DEADLINE, |s| {
        !typing(s).is_empty()
    })
    .await;
    a.execute(Command::SendMessage {
        conversation: conversation.clone(),
        body: "there".into(),
        attachments: Vec::new(),
        reply_to: None,
    })
    .await
    .unwrap();
    wait_for_message(&b, &conversation, "there", "the second message").await;
    assert!(typing(&b.snapshot()).is_empty(), "the message ended it");

    a.shutdown().await;
    b.shutdown().await;
}
