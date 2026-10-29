//! One WebRTC peer with a single Opus track and the `oai-events` data channel.
//!
//! The peer setup follows the approach used by OpenAI's Codex voice helper
//! (https://github.com/openai/codex, Apache-2.0): non-trickle ICE with UDP and TCP
//! host candidates, and explicit remote candidates after the answer is applied.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use rtc::interceptor::Registry;
use rtc::media::Sample;
use rtc::rtp_transceiver::rtp_sender::RTCRtpCodec;
use rtc::rtp_transceiver::rtp_sender::RTCRtpCodecParameters;
use rtc::rtp_transceiver::rtp_sender::RTCRtpCodingParameters;
use rtc::rtp_transceiver::rtp_sender::RTCRtpEncodingParameters;
use rtc::rtp_transceiver::rtp_sender::RtpCodecKind;
use serde_json::Value;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::time::timeout;
use webrtc::data_channel::DataChannel;
use webrtc::data_channel::DataChannelEvent;
use webrtc::media_stream::MediaStreamTrack;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_remote::TrackRemote;
use webrtc::media_stream::track_remote::TrackRemoteEvent;
use webrtc::peer_connection::MediaEngine;
use webrtc::peer_connection::PeerConnection;
use webrtc::peer_connection::PeerConnectionBuilder;
use webrtc::peer_connection::PeerConnectionEventHandler;
use webrtc::peer_connection::RTCIceCandidateInit;
use webrtc::peer_connection::RTCIceGatheringState;
use webrtc::peer_connection::RTCPeerConnectionState;
use webrtc::peer_connection::RTCSessionDescription;

use crate::protocol::Emitter;

pub const OPUS_PAYLOAD_TYPE: u8 = 111;
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REMOTE_CANDIDATES: usize = 32;
const MAX_PENDING_AUDIO_FRAMES: usize = 3000;

/// An RTP packet from the remote audio track.
pub struct IncomingPacket {
    pub sequence: u16,
    pub payload: Vec<u8>,
}

/// An encoded 20 ms Opus frame ready to send.
pub struct OutgoingFrame {
    pub data: Vec<u8>,
}

struct Events {
    gathered: Arc<Notify>,
    incoming: std_mpsc::SyncSender<IncomingPacket>,
    emitter: Emitter,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Events {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathered.notify_one();
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let name = match state {
            RTCPeerConnectionState::New => "new",
            RTCPeerConnectionState::Connecting => "connecting",
            RTCPeerConnectionState::Connected => "connected",
            RTCPeerConnectionState::Disconnected => "disconnected",
            RTCPeerConnectionState::Failed => "failed",
            RTCPeerConnectionState::Closed => "closed",
            _ => "unknown",
        };
        self.emitter
            .emit(serde_json::json!({ "type": "peer", "state": name }));
    }

    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        // Only the locally created event channel is used.
        let _ = channel.close().await;
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let incoming = self.incoming.clone();
        let emitter = self.emitter.clone();
        emitter.emit(serde_json::json!({"type": "remote_track", "state": "opened"}));
        tokio::spawn(async move {
            let mut received = 0u64;
            let mut last_payload_type = None;
            while let Some(event) = track.poll().await {
                match event {
                    TrackRemoteEvent::OnRtpPacket(packet) => {
                        received = received.saturating_add(1);
                        let payload_type = packet.header.payload_type;
                        if last_payload_type != Some(payload_type) {
                            last_payload_type = Some(payload_type);
                            emitter.emit(serde_json::json!({
                                "type": "remote_rtp",
                                "packets": received,
                                "payload_type": payload_type,
                            }));
                        }
                        if packet.header.payload_type != OPUS_PAYLOAD_TYPE {
                            continue;
                        }
                        if received == 1 || received.is_multiple_of(500) {
                            emitter.emit(serde_json::json!({
                                "type": "remote_rtp_progress",
                                "packets": received,
                            }));
                        }
                        // Drop rather than block when the audio engine falls behind.
                        let _ = incoming.try_send(IncomingPacket {
                            sequence: packet.header.sequence_number,
                            payload: packet.payload.to_vec(),
                        });
                    }
                    TrackRemoteEvent::OnEnded => {
                        emitter.emit(serde_json::json!({
                            "type": "remote_track",
                            "state": "ended",
                            "packets": received,
                        }));
                        break;
                    }
                    _ => {}
                }
            }
        });
    }
}

pub struct Transport {
    connection: Arc<dyn PeerConnection>,
    gathered: Arc<Notify>,
    pub ready: watch::Receiver<bool>,
    event_tx: mpsc::Sender<String>,
    observer: tokio::task::JoinHandle<()>,
    sender: tokio::task::JoinHandle<()>,
    event_sender: tokio::task::JoinHandle<()>,
}

impl Transport {
    pub async fn new(
        emitter: Emitter,
        incoming: std_mpsc::SyncSender<IncomingPacket>,
        mut outgoing: mpsc::Receiver<OutgoingFrame>,
    ) -> anyhow::Result<Self> {
        let codec = RTCRtpCodec {
            mime_type: "audio/opus".into(),
            clock_rate: 48_000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".into(),
            rtcp_feedback: vec![],
        };
        let mut media = MediaEngine::default();
        media.register_codec(
            RTCRtpCodecParameters {
                rtp_codec: codec.clone(),
                payload_type: OPUS_PAYLOAD_TYPE,
            },
            RtpCodecKind::Audio,
        )?;
        let ssrc: u32 = rand::random();
        let track = Arc::new(TrackLocalStaticSample::new(MediaStreamTrack::new(
            "gpt-live".into(),
            format!("audio-{ssrc}"),
            "microphone".into(),
            RtpCodecKind::Audio,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec,
                active: true,
                ..Default::default()
            }],
        ))?);

        let gathered = Arc::new(Notify::new());
        let check_interval = Duration::from_millis(200);
        let attempts = (NEGOTIATION_TIMEOUT.as_millis() / check_interval.as_millis()) as u16;
        let mut settings = webrtc::peer_connection::SettingEngine::default();
        settings.set_ice_connection_attempts(Some(check_interval), Some(attempts));

        let runtime = webrtc::runtime::default_runtime()
            .ok_or_else(|| anyhow::anyhow!("no WebRTC runtime available"))?;
        let connection: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::new()
                .with_interceptor_registry(Registry::new())
                .with_media_engine(media)
                .with_runtime(runtime)
                .with_setting_engine(settings)
                .with_handler(Arc::new(Events {
                    gathered: gathered.clone(),
                    incoming,
                    emitter: emitter.clone(),
                }))
                .with_udp_addrs(vec!["0.0.0.0:0", "[::]:0"])
                .with_tcp_addrs(vec!["0.0.0.0:0", "[::]:0"])
                .with_data_channel_send_buffer_limit(64 * 1024)
                .with_sctp_receive_buffer_size(256 * 1024)
                .build()
                .await?,
        );
        connection.add_track(track.clone()).await?;
        let channel = connection.create_data_channel("oai-events", None).await?;

        let (ready_tx, ready) = watch::channel(false);
        let (event_tx, mut event_rx) = mpsc::channel::<String>(32);
        let observer_emitter = emitter.clone();
        let send_channel = Arc::clone(&channel);
        let send_emitter = emitter.clone();
        let event_sender = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                if let Err(error) = send_channel.send_text(&event).await {
                    send_emitter.emit(serde_json::json!({
                        "type": "event_send_error",
                        "message": error.to_string(),
                    }));
                }
            }
        });
        let observer = tokio::spawn(async move {
            while let Some(event) = channel.poll().await {
                match event {
                    DataChannelEvent::OnOpen => {
                        ready_tx.send_replace(true);
                    }
                    DataChannelEvent::OnMessage(message) => {
                        if message.is_string
                            && let Ok(text) = String::from_utf8(message.data.to_vec())
                        {
                            observer_emitter
                                .emit(serde_json::json!({ "type": "event", "data": text }));
                        }
                    }
                    DataChannelEvent::OnError
                    | DataChannelEvent::OnClosing
                    | DataChannelEvent::OnClose => break,
                    _ => {}
                }
            }
            ready_tx.send_replace(false);
        });

        let mut sender_ready = ready.clone();
        let sender_emitter = emitter.clone();
        let sender = tokio::spawn(async move {
            let mut pending = VecDeque::new();
            let mut dropped = 0usize;
            loop {
                tokio::select! {
                    frame = outgoing.recv() => {
                        let Some(frame) = frame else { break };
                        if *sender_ready.borrow() {
                            if !pending.is_empty() {
                                sender_emitter.emit(serde_json::json!({
                                    "type": "audio_context_replay",
                                    "frames": pending.len(),
                                    "dropped_frames": dropped,
                                }));
                                while let Some(buffered) = pending.pop_front() {
                                    write_audio_frame(&track, ssrc, buffered).await;
                                }
                                dropped = 0;
                            }
                            write_audio_frame(&track, ssrc, frame).await;
                        } else {
                            if pending.len() == MAX_PENDING_AUDIO_FRAMES {
                                pending.pop_front();
                                dropped = dropped.saturating_add(1);
                            }
                            pending.push_back(frame);
                        }
                    }
                    changed = sender_ready.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        if *sender_ready.borrow() && !pending.is_empty() {
                            sender_emitter.emit(serde_json::json!({
                                "type": "audio_context_replay",
                                "frames": pending.len(),
                                "dropped_frames": dropped,
                            }));
                            while let Some(frame) = pending.pop_front() {
                                write_audio_frame(&track, ssrc, frame).await;
                            }
                            dropped = 0;
                        }
                    }
                }
            }
        });

        Ok(Self {
            connection,
            gathered,
            ready,
            event_tx,
            observer,
            sender,
            event_sender,
        })
    }

    pub async fn send_event(&self, event: Value) -> anyhow::Result<()> {
        let serialized = serde_json::to_string(&event)?;
        self.event_tx
            .send(serialized)
            .await
            .map_err(|_| anyhow::anyhow!("GPT-Live event sender is closed"))
    }

    pub async fn offer(&self) -> anyhow::Result<String> {
        timeout(NEGOTIATION_TIMEOUT, async {
            let offer = self.connection.create_offer(None).await?;
            self.connection.set_local_description(offer).await?;
            self.gathered.notified().await;
            self.connection
                .local_description()
                .await
                .map(|offer| offer.sdp)
                .ok_or_else(|| anyhow::anyhow!("missing local offer"))
        })
        .await
        .map_err(|_| anyhow::anyhow!("timed out gathering ICE candidates"))?
    }

    pub async fn apply_answer(&self, sdp: String) -> anyhow::Result<()> {
        let answer = RTCSessionDescription::answer(sdp)?;
        let parsed = answer.unmarshal()?;
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        for (index, attribute) in parsed
            .media_descriptions
            .iter()
            .flat_map(|media| &media.attributes)
            .filter(|attribute| attribute.key == "candidate")
            .enumerate()
        {
            if index >= MAX_REMOTE_CANDIDATES {
                anyhow::bail!("answer contains too many ICE candidates");
            }
            let Some(candidate) = attribute.value.as_deref() else {
                continue;
            };
            let Ok(parsed) = rtc::ice::candidate::unmarshal_candidate(candidate) else {
                continue;
            };
            if parsed.component() == 1 && seen.insert(candidate.to_string()) {
                candidates.push(RTCIceCandidateInit {
                    candidate: format!("candidate:{candidate}"),
                    ..Default::default()
                });
            }
        }
        timeout(NEGOTIATION_TIMEOUT, async {
            self.connection.set_remote_description(answer).await?;
            // Bundled candidates must be added explicitly to start TCP dialing.
            for candidate in candidates {
                self.connection.add_ice_candidate(candidate).await?;
            }
            self.ready
                .clone()
                .wait_for(|ready| *ready)
                .await
                .map(|_| ())
                .map_err(|_| anyhow::anyhow!("event channel closed during negotiation"))
        })
        .await
        .map_err(|_| anyhow::anyhow!("timed out connecting to GPT-Live"))?
    }

    pub async fn close(self) {
        self.observer.abort();
        self.sender.abort();
        self.event_sender.abort();
        let _ = timeout(Duration::from_secs(2), self.connection.close()).await;
    }
}

async fn write_audio_frame(track: &TrackLocalStaticSample, ssrc: u32, frame: OutgoingFrame) {
    let sample = Sample {
        data: frame.data.into(),
        duration: Duration::from_millis(20),
        ..Default::default()
    };
    let write = track.write_sample(ssrc, OPUS_PAYLOAD_TYPE, &sample, &[]);
    if let Err(error) = timeout(Duration::from_millis(200), write).await {
        eprintln!("gpt-live-host: outbound audio write stalled: {error}");
    }
}
