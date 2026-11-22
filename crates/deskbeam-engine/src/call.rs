//! Voice calls with paired devices (plan Phase 6a).
//!
//! - Calls follow the "Chat" permission: a device may call this one, and be called, when it
//!   is paired and allowed to chat. Incoming calls always ring and wait for the user.
//! - One call at a time; a call to a busy device gets `Busy`.
//! - Setup runs on one bi-stream on `deskbeam/call/1`; the audio goes as datagrams on the
//!   same connection (`deskbeam_call::media`). A reader task turns the stream into a
//!   channel, so waiting on it next to other things never cuts a frame in half.

use std::{
    sync::{Arc, Weak},
    time::Duration,
};

use deskbeam_call::{
    ALPN, Frame,
    devices::Devices,
    media::{Media, Options},
    read_frame,
    video::{
        Profile, VideoFrame,
        camera::Webcam,
        screen::{Screen, ShareSource, ask_to_capture, may_capture, share_sources},
        source::VideoSource,
        stream::{Latest, VideoReceiver, VideoSender},
    },
    write_frame,
};
use deskbeam_model::{Call, CallState, CameraQuality, ShareOption, ShareSources, ShareTarget};
use iroh::{
    EndpointId,
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler},
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::debug;

use crate::{
    Event,
    command::CommandError,
    engine::{Engine, Inner, paired_id, unix_ms},
};

/// How long connecting and hearing the offer may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// How long an ended call stays in the snapshot, to show why.
const SHOW_ENDED: Duration = Duration::from_secs(4);
/// How often a running call's figures are refreshed.
const STATS_EVERY: Duration = Duration::from_millis(250);

/// The audio devices calls use: `None` for the system's, chosen in the audio settings, or
/// a stand-in for tests.
#[derive(Clone, Default)]
pub struct CallAudio(pub Option<std::sync::Arc<dyn Devices>>);

impl std::fmt::Debug for CallAudio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CallAudio")
    }
}

/// The camera calls use: `None` for the system's, chosen in the settings, or a stand-in
/// for tests.
#[derive(Clone, Default)]
pub struct CallCamera(pub Option<Arc<dyn VideoSource>>);

impl std::fmt::Debug for CallCamera {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CallCamera")
    }
}

/// The running call's pictures, for the window: the newest of the other end's, and of this
/// end's own camera. They don't go through the snapshot.
#[derive(Default, Clone)]
pub struct CallVideo {
    theirs: Option<Arc<Latest>>,
    mine: Option<Arc<Latest>>,
}

impl CallVideo {
    /// Their newest picture and its number (which grows with each new one).
    pub fn theirs(&self) -> Option<(u64, Arc<VideoFrame>)> {
        self.theirs.as_ref().and_then(|l| l.get())
    }

    /// This device's own camera, as sent.
    pub fn mine(&self) -> Option<(u64, Arc<VideoFrame>)> {
        self.mine.as_ref().and_then(|l| l.get())
    }
}

/// A datagram's size when the path does not say (relays allow about this).
const DATAGRAM: usize = 1100;

/// What the user asks of the call running now.
enum Control {
    Answer(bool),
    Hangup,
    Mute(bool),
    Camera(bool),
    /// Share this (with its name), or stop sharing.
    Share(Option<(ShareTarget, String)>),
}

/// What this end sends as video.
#[derive(Debug, Clone, PartialEq)]
enum Showing {
    Camera,
    Screen(ShareTarget, String),
}

/// What `call.listShareSources` found, with the pictures the snapshot leaves out.
pub(crate) struct Shareable {
    allowed: bool,
    options: Vec<(ShareOption, Option<Arc<VideoFrame>>)>,
}

/// Width of the pictures in the share picker.
const THUMBNAIL: usize = 240;

fn to_source(target: &ShareTarget) -> ShareSource {
    match target {
        ShareTarget::Display { id } => ShareSource::Display { id: *id },
        ShareTarget::Window { id } => ShareSource::Window { id: *id },
    }
}

fn to_target(source: &ShareSource) -> ShareTarget {
    match source {
        ShareSource::Display { id } => ShareTarget::Display { id: *id },
        ShareSource::Window { id } => ShareTarget::Window { id: *id },
    }
}

/// The call this device is in, ringing or just ended.
pub(crate) struct Slot {
    info: Call,
    control: Option<mpsc::UnboundedSender<Control>>,
}

impl Engine {
    pub(crate) fn call_view(&self) -> Option<Call> {
        self.inner
            .call
            .lock()
            .expect("lock")
            .as_ref()
            .map(|s| s.info.clone())
    }

    /// Changes the call's view, if it is still call `id`.
    fn update_call(&self, id: &str, f: impl FnOnce(&mut Call)) {
        if let Some(slot) = self.inner.call.lock().expect("lock").as_mut()
            && slot.info.id == id
        {
            f(&mut slot.info);
        }
    }

    /// Whether `peer` (lowercase) is paired and may chat, which is what calls need.
    fn may_call(&self, peer: &str) -> bool {
        self.paired_ids().contains(peer) && self.inner.store.permissions(peer).is_ok_and(|p| p.chat)
    }

    pub(crate) async fn call_start(&self, endpoint_id: &str) -> Result<Value, CommandError> {
        let node = self.node().await?;
        let id = paired_id(&node, endpoint_id)?;
        let peer = id.to_lowercase();
        let device = node
            .list_paired()?
            .into_iter()
            .find(|d| d.endpoint_id.eq_ignore_ascii_case(&id));
        let name = device
            .as_ref()
            .map(|d| self.local_name(&d.endpoint_id, d.display_name.clone()))
            .unwrap_or_else(|| deskbeam_model::short_id(&id).to_string());
        if !self.may_call(&peer) {
            return Err(CommandError::Failed(format!(
                "chat with {name} is turned off, and calls go with it"
            )));
        }
        if !device.is_some_and(|d| d.online) {
            return Err(CommandError::Failed(format!("{name} is offline")));
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let call_id = deskbeam_chat::new_id();
        {
            let mut slot = self.inner.call.lock().expect("lock");
            if slot.as_ref().is_some_and(|s| s.info.is_live()) {
                return Err(CommandError::Failed("already in a call".into()));
            }
            *slot = Some(Slot {
                info: new_call(&call_id, &peer, &name, true, CallState::Calling),
                control: Some(tx),
            });
        }
        self.refresh().await;
        let engine = self.clone();
        let id = call_id.clone();
        tokio::spawn(async move {
            let reason = engine.outgoing(&peer, &id, &name, rx).await;
            engine.end_call(&id, reason).await;
        });
        Ok(json!({ "id": call_id }))
    }

    fn call_control(&self, control: Control) -> Result<Value, CommandError> {
        let slot = self.inner.call.lock().expect("lock");
        let tx = slot
            .as_ref()
            .filter(|s| s.info.is_live())
            .and_then(|s| s.control.clone())
            .ok_or_else(|| CommandError::Failed("there is no call".into()))?;
        drop(slot);
        let _ = tx.send(control);
        Ok(Value::Null)
    }

    pub(crate) fn call_answer(&self, accept: bool) -> Result<Value, CommandError> {
        let ringing = self
            .call_view()
            .is_some_and(|c| c.state == CallState::Ringing);
        if !ringing {
            return Err(CommandError::Failed("no call is ringing".into()));
        }
        self.call_control(Control::Answer(accept))
    }

    pub(crate) fn call_hangup(&self) -> Result<Value, CommandError> {
        self.call_control(Control::Hangup)
    }

    /// The running call's pictures, for the window to show.
    pub fn call_video(&self) -> CallVideo {
        self.inner.video.lock().expect("lock").clone()
    }

    pub(crate) fn call_camera(&self, on: bool) -> Result<Value, CommandError> {
        let call = self
            .call_view()
            .filter(|c| c.state == CallState::Active)
            .ok_or_else(|| CommandError::Failed("there is no call".into()))?;
        if on && !call.video {
            return Err(CommandError::Failed(format!(
                "{}'s DeskBeam can't do video yet: it needs updating",
                call.display_name
            )));
        }
        self.call_control(Control::Camera(on))
    }

    pub(crate) fn call_share(&self, target: Option<ShareTarget>) -> Result<Value, CommandError> {
        let call = self
            .call_view()
            .filter(|c| c.state == CallState::Active)
            .ok_or_else(|| CommandError::Failed("there is no call".into()))?;
        let Some(target) = target else {
            return self.call_control(Control::Share(None));
        };
        if !call.video {
            return Err(CommandError::Failed(format!(
                "{}'s DeskBeam can't show a shared screen yet: it needs updating",
                call.display_name
            )));
        }
        let name = self
            .inner
            .share_sources
            .lock()
            .expect("lock")
            .as_ref()
            .and_then(|s| s.options.iter().find(|(o, _)| o.target == target))
            .map(|(o, _)| o.name.clone())
            .unwrap_or_else(|| "your screen".into());
        self.call_control(Control::Share(Some((target, name))))
    }

    pub(crate) async fn list_share_sources(&self) -> Result<Value, CommandError> {
        let testing = self.inner.call_screen.0.is_some();
        let (allowed, choices) = tokio::task::spawn_blocking(move || {
            if testing {
                (true, Vec::new())
            } else {
                (may_capture(), share_sources(THUMBNAIL))
            }
        })
        .await
        .map_err(|e| CommandError::Failed(e.to_string()))?;
        let options: Vec<(ShareOption, Option<Arc<VideoFrame>>)> = choices
            .into_iter()
            .map(|c| {
                (
                    ShareOption {
                        target: to_target(&c.source),
                        name: c.name,
                        width: c.width,
                        height: c.height,
                    },
                    c.thumbnail.map(Arc::new),
                )
            })
            .collect();
        let found = options.len();
        *self.inner.share_sources.lock().expect("lock") = Some(Shareable { allowed, options });
        Ok(json!({ "allowed": allowed, "found": found }))
    }

    pub(crate) fn share_sources_view(&self) -> Option<ShareSources> {
        self.inner
            .share_sources
            .lock()
            .expect("lock")
            .as_ref()
            .map(|s| ShareSources {
                allowed: s.allowed,
                options: s.options.iter().map(|(o, _)| o.clone()).collect(),
            })
    }

    /// The small picture of something that can be shared, from the last listing.
    pub fn share_thumbnail(&self, target: &ShareTarget) -> Option<Arc<VideoFrame>> {
        self.inner
            .share_sources
            .lock()
            .expect("lock")
            .as_ref()
            .and_then(|s| s.options.iter().find(|(o, _)| o.target == *target))
            .and_then(|(_, t)| t.clone())
    }

    /// The camera to open: the one given in the config (tests), or the system's, by the name
    /// chosen in the settings.
    fn camera_source(&self) -> Arc<dyn VideoSource> {
        match &self.inner.call_camera.0 {
            Some(source) => source.clone(),
            None => {
                let settings = self.audio_settings();
                Arc::new(Webcam {
                    name: settings.camera,
                    size: match settings.camera_quality {
                        CameraQuality::Standard => Webcam::STANDARD,
                        CameraQuality::Hd => Webcam::HD,
                    },
                })
            }
        }
    }

    pub(crate) async fn call_mute(&self, on: bool) -> Result<Value, CommandError> {
        let result = self.call_control(Control::Mute(on))?;
        if let Some(call) = self.call_view() {
            self.update_call(&call.id, |c| c.muted = on);
        }
        self.refresh().await;
        Ok(result)
    }

    /// Rings `peer` and runs the call. Returns why it ended.
    async fn outgoing(
        &self,
        peer: &str,
        id: &str,
        name: &str,
        mut control: mpsc::UnboundedReceiver<Control>,
    ) -> String {
        let setup = async {
            let node = self.node().await?;
            let remote: EndpointId = peer.parse()?;
            let endpoint = node.endpoint().await;
            let connection =
                tokio::time::timeout(CONNECT_TIMEOUT, endpoint.connect(remote, ALPN)).await??;
            let (mut send, recv) = connection.open_bi().await?;
            write_frame(
                &mut send,
                &Frame::Offer {
                    id: id.to_string(),
                    video: true,
                },
            )
            .await?;
            anyhow::Ok((connection, send, recv))
        };
        let (connection, mut send, recv) = match setup.await {
            Ok(parts) => parts,
            Err(e) => {
                debug!(%peer, "call not placed: {e:#}");
                return format!("{name} could not be reached");
            }
        };
        let mut frames = reader(recv);
        let deadline = tokio::time::sleep(self.inner.ring_timeout + CONNECT_TIMEOUT);
        tokio::pin!(deadline);
        let video = loop {
            tokio::select! {
                frame = frames.recv() => match frame {
                    Some(Frame::Accept { video }) => break video,
                    Some(Frame::Decline { reason }) => {
                        connection.close(0u32.into(), b"declined");
                        return if reason.is_empty() {
                            format!("{name} declined")
                        } else {
                            format!("{name} declined: {reason}")
                        };
                    }
                    Some(Frame::Busy) => {
                        connection.close(0u32.into(), b"busy");
                        return format!("{name} is in another call");
                    }
                    Some(Frame::Hangup) | None => return format!("{name} did not answer"),
                    Some(_) => {}
                },
                asked = control.recv() => match asked {
                    Some(Control::Hangup) | None => {
                        let _ = write_frame(&mut send, &Frame::Hangup).await;
                        let _ = send.finish();
                        connection.close(0u32.into(), b"cancelled");
                        return "Cancelled".into();
                    }
                    Some(_) => {}
                },
                () = &mut deadline => {
                    let _ = write_frame(&mut send, &Frame::Hangup).await;
                    connection.close(0u32.into(), b"no answer");
                    return format!("{name} did not answer");
                }
            }
        };
        self.talk(id, name, connection, send, frames, control, video)
            .await
    }

    /// Answers a call that came in, once the user has. Returns why it ended, and whether it
    /// was missed.
    async fn incoming(
        &self,
        connection: Connection,
        mut send: SendStream,
        recv: RecvStream,
        peer: String,
    ) {
        let mut frames = reader(recv);
        let offer = tokio::time::timeout(CONNECT_TIMEOUT, frames.recv()).await;
        let Ok(Some(Frame::Offer { id, video })) = offer else {
            return;
        };
        if !self.may_call(&peer) {
            debug!(%peer, "call from a device that may not call refused");
            let _ = write_frame(
                &mut send,
                &Frame::Decline {
                    reason: "calls are not allowed".into(),
                },
            )
            .await;
            let _ = send.finish();
            return;
        }
        let name = self
            .names()
            .get(&peer)
            .cloned()
            .unwrap_or_else(|| deskbeam_model::short_id(&peer).to_string());
        let (tx, mut control) = mpsc::unbounded_channel();
        let busy = {
            let mut slot = self.inner.call.lock().expect("lock");
            let busy = slot.as_ref().is_some_and(|s| s.info.is_live());
            if !busy {
                *slot = Some(Slot {
                    info: new_call(&id, &peer, &name, false, CallState::Ringing),
                    control: Some(tx),
                });
            }
            busy
        };
        if busy {
            let _ = write_frame(&mut send, &Frame::Busy).await;
            let _ = send.finish();
            return;
        }
        let _ = self.inner.events.send(Event::CallIncoming {
            endpoint_id: peer.clone(),
            display_name: name.clone(),
        });
        self.refresh().await;
        let _ = write_frame(&mut send, &Frame::Ringing).await;

        let missed = |engine: &Engine| {
            let _ = engine.inner.events.send(Event::CallMissed {
                endpoint_id: peer.clone(),
                display_name: name.clone(),
            });
        };
        let ring = tokio::time::sleep(self.inner.ring_timeout);
        tokio::pin!(ring);
        let answered = loop {
            tokio::select! {
                frame = frames.recv() => match frame {
                    Some(Frame::Hangup) | None => {
                        missed(self);
                        break Err(format!("Missed call from {name}"));
                    }
                    Some(_) => {}
                },
                asked = control.recv() => match asked {
                    Some(Control::Answer(true)) => break Ok(()),
                    Some(Control::Answer(false)) | Some(Control::Hangup) | None => {
                        let _ = write_frame(&mut send, &Frame::Decline { reason: String::new() }).await;
                        let _ = send.finish();
                        break Err("Declined".to_string());
                    }
                    Some(Control::Mute(_) | Control::Camera(_) | Control::Share(_)) => {}
                },
                () = &mut ring => {
                    let _ = write_frame(&mut send, &Frame::Decline { reason: "no answer".into() }).await;
                    let _ = send.finish();
                    missed(self);
                    break Err(format!("Missed call from {name}"));
                }
            }
        };
        let reason = match answered {
            Err(reason) => reason,
            Ok(()) => {
                if let Err(e) = write_frame(&mut send, &Frame::Accept { video: true }).await {
                    debug!("accepting the call: {e:#}");
                    format!("{name} hung up")
                } else {
                    self.talk(&id, &name, connection, send, frames, control, video)
                        .await
                }
            }
        };
        self.end_call(&id, reason).await;
    }

    /// The call is on: audio both ways, and video when both ends can and a camera is on,
    /// until either end hangs up.
    #[allow(clippy::too_many_arguments)]
    async fn talk(
        &self,
        id: &str,
        name: &str,
        connection: Connection,
        mut send: SendStream,
        mut frames: mpsc::UnboundedReceiver<Frame>,
        mut control: mpsc::UnboundedReceiver<Control>,
        video: bool,
    ) -> String {
        self.update_call(id, |c| {
            c.state = CallState::Active;
            c.started_at = Some(unix_ms());
            c.video = video;
        });
        self.refresh().await;

        // The call takes the microphone from an audio test.
        self.stop_audio_test();
        let devices = self.audio_devices_for_call();
        let options = Options {
            processing: Some(crate::audio::processing(&self.audio_settings())),
            // A diagnostic: DESKBEAM_CALL_RECORD=<folder> keeps each call's audio there.
            record: std::env::var_os("DESKBEAM_CALL_RECORD").map(Into::into),
        };
        let outgoing = connection.clone();
        let started = tokio::task::spawn_blocking(move || {
            Media::start(devices.as_ref(), options, move |packet| {
                // A full send buffer drops the packet; live audio can't wait.
                let _ = outgoing.send_datagram(packet.into());
            })
        })
        .await;
        let media = match started {
            Ok(Ok(media)) => std::sync::Arc::new(media),
            Ok(Err(e)) => {
                let _ = write_frame(&mut send, &Frame::Hangup).await;
                connection.close(0u32.into(), b"no audio");
                return format!("The microphone or speaker did not open: {e}");
            }
            Err(e) => {
                connection.close(0u32.into(), b"no audio");
                return format!("The call's audio did not start: {e}");
            }
        };
        self.inner.audio.lock().expect("lock").call = Some(std::sync::Arc::downgrade(&media));
        // Their video is received whenever both ends can do it; it shows once they turn
        // their camera on.
        let watching = if video {
            match VideoReceiver::start() {
                Ok(receiver) => Some(Arc::new(receiver)),
                Err(e) => {
                    debug!("no video receiver: {e:#}");
                    None
                }
            }
        } else {
            None
        };
        self.inner.video.lock().expect("lock").theirs =
            watching.as_ref().map(|w| w.latest().clone());
        let (heard, seen, incoming) = (media.clone(), watching.clone(), connection.clone());
        let receiving = tokio::spawn(async move {
            while let Ok(datagram) = incoming.read_datagram().await {
                if seen.as_ref().is_some_and(|v| v.receive(&datagram)) {
                    continue;
                }
                heard.receive(&datagram);
            }
        });
        // What is sent as video: a share comes before the camera. The one sending keeps
        // going until its replacement has opened.
        let mut sending: Option<(Showing, VideoSender)> = None;
        let mut opening: Option<Showing> = None;
        let mut want_camera = false;
        let mut want_share: Option<(ShareTarget, String)> = None;
        let (opened_tx, mut opened) =
            mpsc::unbounded_channel::<(Showing, anyhow::Result<VideoSender>)>();
        let mut ticks = 0u64;
        if let Some(call) = self.call_view()
            && call.muted
        {
            media.set_muted(true);
            let _ = write_frame(&mut send, &Frame::Mute { on: true }).await;
        }

        let mut tick = tokio::time::interval(STATS_EVERY);
        let reason = loop {
            tokio::select! {
                frame = frames.recv() => match frame {
                    Some(Frame::Mute { on }) => {
                        self.update_call(id, |c| c.peer_muted = on);
                        self.refresh().await;
                    }
                    Some(Frame::Hangup) => break format!("{name} hung up"),
                    Some(Frame::Video { on }) => {
                        self.update_call(id, |c| {
                            c.peer_camera = on;
                            if !on {
                                c.peer_sharing = false;
                            }
                        });
                        if !on && let Some(w) = &watching {
                            w.latest().clear();
                        }
                        self.refresh().await;
                    }
                    Some(Frame::Share { on }) => {
                        self.update_call(id, |c| c.peer_sharing = on);
                        self.refresh().await;
                    }
                    Some(Frame::KeyframeRequest) => {
                        if let Some((_, sender)) = &sending {
                            sender.request_keyframe();
                        }
                    }
                    Some(Frame::VideoReport { loss }) => {
                        if let Some((_, sender)) = &sending {
                            sender.report(loss as f32 / 100.0);
                        }
                    }
                    Some(_) => {}
                    None => break "The connection was lost".to_string(),
                },
                asked = control.recv() => match asked {
                    Some(Control::Mute(on)) => {
                        media.set_muted(on);
                        let _ = write_frame(&mut send, &Frame::Mute { on }).await;
                    }
                    Some(Control::Camera(on)) => {
                        want_camera = on && video;
                        self.update_call(id, |c| {
                            c.camera = want_camera;
                            c.camera_error = None;
                        });
                        self.steer_video(
                            id, video, &connection, &mut send, &mut sending, &mut opening,
                            want_camera, &want_share, &opened_tx,
                        )
                        .await;
                        self.refresh().await;
                    }
                    Some(Control::Share(share)) => {
                        want_share = share.filter(|_| video);
                        self.update_call(id, |c| c.share_error = None);
                        self.steer_video(
                            id, video, &connection, &mut send, &mut sending, &mut opening,
                            want_camera, &want_share, &opened_tx,
                        )
                        .await;
                        self.refresh().await;
                    }
                    Some(Control::Hangup) | None => {
                        let _ = write_frame(&mut send, &Frame::Hangup).await;
                        let _ = send.finish();
                        break "Call ended".to_string();
                    }
                    Some(Control::Answer(_)) => {}
                },
                Some((kind, started)) = opened.recv() => {
                    if opening.as_ref() == Some(&kind) {
                        opening = None;
                    }
                    let wanted = wanted_video(want_camera, &want_share);
                    match started {
                        Ok(sender) if wanted.as_ref() == Some(&kind) => {
                            self.inner.video.lock().expect("lock").mine =
                                Some(sender.preview().clone());
                            let was = sending.replace((kind.clone(), sender));
                            if let Some((_, old)) = was {
                                tokio::task::spawn_blocking(move || drop(old));
                            }
                            let screen = matches!(kind, Showing::Screen(..));
                            let _ = write_frame(&mut send, &Frame::Video { on: true }).await;
                            let _ = write_frame(&mut send, &Frame::Share { on: screen }).await;
                        }
                        // No longer wanted by the time it opened.
                        Ok(sender) => {
                            tokio::task::spawn_blocking(move || drop(sender));
                        }
                        Err(e) => match kind {
                            Showing::Camera => {
                                want_camera = false;
                                self.update_call(id, |c| {
                                    c.camera = false;
                                    c.camera_error =
                                        Some(format!("The camera did not open: {e:#}"));
                                });
                            }
                            Showing::Screen(..) => {
                                want_share = None;
                                self.update_call(id, |c| {
                                    c.share_error = Some(format!("Sharing did not start: {e:#}"));
                                });
                            }
                        },
                    }
                    self.steer_video(
                        id, video, &connection, &mut send, &mut sending, &mut opening,
                        want_camera, &want_share, &opened_tx,
                    )
                    .await;
                    self.refresh().await;
                }
                _ = tick.tick() => {
                    ticks += 1;
                    let stats = media.stats();
                    self.update_call(id, |c| {
                        c.loss = stats.loss();
                        c.level = stats.level;
                    });
                    let peer_camera = self.call_view().is_some_and(|c| c.peer_camera);
                    if let Some(w) = &watching && peer_camera {
                        if w.wants_keyframe() {
                            let _ = write_frame(&mut send, &Frame::KeyframeRequest).await;
                        }
                        if ticks.is_multiple_of(4) {
                            let loss = (w.loss_report() * 100.0).round().clamp(0.0, 100.0) as u8;
                            let _ = write_frame(&mut send, &Frame::VideoReport { loss }).await;
                        }
                    }
                    self.refresh().await;
                }
            }
        };
        receiving.abort();
        let _ = receiving.await;
        *self.inner.video.lock().expect("lock") = CallVideo::default();
        if let Some((_, sender)) = sending.take() {
            tokio::task::spawn_blocking(move || drop(sender));
        }
        // The last handle: the loop stops and the devices close here.
        drop(media);
        connection.close(0u32.into(), b"ended");
        reason
    }

    /// Starts, switches or stops what this end sends as video, after any change of what is
    /// wanted: a share first, then the camera, else nothing. Opening runs on a blocking
    /// thread and reports to the call loop through `opened`.
    #[allow(clippy::too_many_arguments)]
    async fn steer_video(
        &self,
        id: &str,
        video: bool,
        connection: &Connection,
        send: &mut SendStream,
        sending: &mut Option<(Showing, VideoSender)>,
        opening: &mut Option<Showing>,
        want_camera: bool,
        want_share: &Option<(ShareTarget, String)>,
        opened: &mpsc::UnboundedSender<(Showing, anyhow::Result<VideoSender>)>,
    ) {
        let wanted = if video {
            wanted_video(want_camera, want_share)
        } else {
            None
        };
        let current = sending.as_ref().map(|(kind, _)| kind.clone());
        match &wanted {
            None => {
                if let Some((kind, sender)) = sending.take() {
                    self.inner.video.lock().expect("lock").mine = None;
                    tokio::task::spawn_blocking(move || drop(sender));
                    if matches!(kind, Showing::Screen(..)) {
                        let _ = write_frame(send, &Frame::Share { on: false }).await;
                    }
                    let _ = write_frame(send, &Frame::Video { on: false }).await;
                }
            }
            Some(kind) if current.as_ref() != Some(kind) && opening.as_ref() != Some(kind) => {
                let source: Arc<dyn VideoSource> = match kind {
                    Showing::Camera => self.camera_source(),
                    Showing::Screen(target, _) => match &self.inner.call_screen.0 {
                        Some(test) => test.clone(),
                        None => {
                            if !may_capture() {
                                // The system's question, or its settings page after that.
                                tokio::task::spawn_blocking(ask_to_capture);
                                let tx = opened.clone();
                                let _ = tx.send((
                                    kind.clone(),
                                    Err(anyhow::anyhow!(
                                        "allow DeskBeam in System Settings → Privacy & Security \
                                         → Screen Recording, then quit and reopen DeskBeam"
                                    )),
                                ));
                                *opening = Some(kind.clone());
                                self.show_video_state(id, sending, opening);
                                return;
                            }
                            Arc::new(Screen {
                                source: to_source(target),
                                profile: Profile::SCREEN,
                            })
                        }
                    },
                };
                let profile = match kind {
                    Showing::Camera => match self.audio_settings().camera_quality {
                        CameraQuality::Standard => Profile::CAMERA,
                        CameraQuality::Hd => Profile::CAMERA_HD,
                    },
                    Showing::Screen(..) => Profile::SCREEN,
                };
                *opening = Some(kind.clone());
                let (path, out, tx, kind) = (
                    connection.clone(),
                    connection.clone(),
                    opened.clone(),
                    kind.clone(),
                );
                tokio::task::spawn_blocking(move || {
                    let started = VideoSender::start(
                        source.as_ref(),
                        profile,
                        move || path.max_datagram_size().unwrap_or(DATAGRAM).min(1200),
                        move |datagram| {
                            let _ = out.send_datagram(datagram.into());
                        },
                    );
                    let _ = tx.send((kind, started));
                });
            }
            Some(_) => {}
        }
        self.show_video_state(id, sending, opening);
    }

    /// The call's view of what this end sends.
    fn show_video_state(
        &self,
        id: &str,
        sending: &Option<(Showing, VideoSender)>,
        opening: &Option<Showing>,
    ) {
        self.update_call(id, |c| {
            c.sharing = match sending {
                Some((Showing::Screen(_, name), _)) => Some(name.clone()),
                _ => None,
            };
            c.camera_opening = matches!(opening, Some(Showing::Camera));
            c.share_opening = matches!(opening, Some(Showing::Screen(..)));
        });
    }

    /// Shows why the call ended for a few seconds, then clears it.
    async fn end_call(&self, id: &str, reason: String) {
        if let Some(slot) = self.inner.call.lock().expect("lock").as_mut()
            && slot.info.id == id
        {
            slot.info.state = CallState::Ended { reason };
            slot.control = None;
        }
        self.refresh().await;
        let engine = self.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(SHOW_ENDED).await;
            let cleared = {
                let mut slot = engine.inner.call.lock().expect("lock");
                let ended = slot
                    .as_ref()
                    .is_some_and(|s| s.info.id == id && !s.info.is_live());
                if ended {
                    *slot = None;
                }
                ended
            };
            if cleared {
                engine.refresh().await;
            }
        });
    }
}

/// What should be sent: a share before the camera.
fn wanted_video(want_camera: bool, want_share: &Option<(ShareTarget, String)>) -> Option<Showing> {
    match want_share {
        Some((target, name)) => Some(Showing::Screen(target.clone(), name.clone())),
        None => want_camera.then_some(Showing::Camera),
    }
}

fn new_call(id: &str, peer: &str, name: &str, outgoing: bool, state: CallState) -> Call {
    Call {
        id: id.to_string(),
        endpoint_id: peer.to_string(),
        display_name: name.to_string(),
        outgoing,
        state,
        muted: false,
        peer_muted: false,
        started_at: None,
        loss: 0.0,
        level: 0.0,
        video: false,
        camera: false,
        camera_opening: false,
        peer_camera: false,
        camera_error: None,
        sharing: None,
        share_opening: false,
        peer_sharing: false,
        share_error: None,
    }
}

/// Reads frames into a channel until the stream ends or fails.
fn reader(mut recv: RecvStream) -> mpsc::UnboundedReceiver<Frame> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(frame) = read_frame(&mut recv).await {
            if tx.send(frame).is_err() {
                break;
            }
        }
    });
    rx
}

/// The callee side of the call protocol, on the node's endpoint.
pub(crate) struct CallServer {
    inner: Weak<Inner>,
}

impl CallServer {
    pub(crate) fn new(inner: Weak<Inner>) -> Self {
        Self { inner }
    }
}

impl std::fmt::Debug for CallServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CallServer")
    }
}

impl ProtocolHandler for CallServer {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let peer = connection.remote_id().to_string().to_lowercase();
        let Some(inner) = self.inner.upgrade() else {
            return Ok(());
        };
        let engine = Engine { inner };
        if !engine.paired_ids().contains(&peer) {
            debug!(%peer, "call link from a device that is not paired refused");
            connection.close(1u32.into(), b"not paired");
            return Ok(());
        }
        let Ok((send, recv)) = connection.accept_bi().await else {
            return Ok(());
        };
        engine.incoming(connection.clone(), send, recv, peer).await;
        // Let the last frames reach the caller before the connection goes.
        tokio::time::sleep(Duration::from_millis(200)).await;
        connection.close(0u32.into(), b"done");
        Ok(())
    }
}
