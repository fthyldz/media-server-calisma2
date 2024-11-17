use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

use futures::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{
        mpsc::{self, UnboundedSender},
        Mutex, RwLock,
    },
};
use tokio_tungstenite::{tungstenite::Message, WebSocketStream};
use webrtc::{
    api::{interceptor_registry, media_engine::MediaEngine, APIBuilder},
    ice_transport::{
        ice_candidate::{RTCIceCandidate, RTCIceCandidateInit},
        ice_server::RTCIceServer,
    },
    interceptor::registry::Registry,
    peer_connection::{
        configuration::RTCConfiguration, policy::ice_transport_policy::RTCIceTransportPolicy,
        sdp::session_description::RTCSessionDescription, RTCPeerConnection,
    },
    rtp_transceiver::{
        rtp_codec::{RTCRtpCodecCapability, RTPCodecType},
        rtp_sender,
        rtp_transceiver_direction::RTCRtpTransceiverDirection,
        RTCRtpCodingParameters, RTCRtpTransceiverInit,
    },
    stun::{addr, client::Client},
    track::{
        track_local::{track_local_static_rtp::TrackLocalStaticRTP, TrackLocalWriter},
        track_remote::TrackRemote,
    },
};

#[derive(Default)]
struct Room {
    peers: HashMap<String, Arc<RTCPeerConnection>>,
    tracks: Vec<Arc<TrackLocalStaticRTP>>,
}

type WebSocketClients =
    Arc<RwLock<HashMap<SocketAddr, Arc<Mutex<mpsc::UnboundedSender<Message>>>>>>;
type WebRTCClients = Arc<RwLock<HashMap<SocketAddr, Arc<Mutex<RTCPeerConnection>>>>>;

#[tokio::main]
async fn main() {
    let web_socket_clients: WebSocketClients = Arc::new(RwLock::new(HashMap::new()));
    let webrtc_clients: WebRTCClients = Arc::new(RwLock::new(HashMap::new()));
    start_listening(web_socket_clients, webrtc_clients).await;
}

async fn start_listening(web_socket_clients: WebSocketClients, webrtc_clients: WebRTCClients) {
    let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 8080);
    if let Ok(listener) = TcpListener::bind(&server_addr).await {
        println!("WebSocket server is running at: {}", server_addr);
        loop {
            if let Ok((stream, addr)) = listener.accept().await {
                tokio::spawn(handle_connection(
                    stream,
                    addr,
                    web_socket_clients.clone(),
                    webrtc_clients.clone(),
                ));
            } else {
                println!("Failed to accept connection");
            }
        }
    } else {
        println!("Failed to bind to address: {}", server_addr);
    }
}

async fn handle_connection(
    stream: TcpStream,
    addr: SocketAddr,
    web_socket_clients: WebSocketClients,
    webrtc_clients: WebRTCClients,
) {
    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            println!("WebSocket error: {}", e);
            return;
        }
    };
    let (ws_sender, ws_receiver) = ws_stream.split();
    let (tx, rx) = mpsc::unbounded_channel::<Message>();
    web_socket_clients
        .write()
        .await
        .insert(addr, Arc::new(Mutex::new(tx)));
    tokio::spawn(create_sender(rx, ws_sender));
    tokio::spawn(handle_messages(
        web_socket_clients.clone(),
        webrtc_clients.clone(),
        addr,
        ws_receiver,
    ));
}

async fn create_sender(
    mut rx: mpsc::UnboundedReceiver<Message>,
    mut ws_sender: SplitSink<WebSocketStream<TcpStream>, Message>,
) {
    while let Some(msg) = rx.recv().await {
        let _ = ws_sender.send(msg).await;
    }
}

async fn handle_messages(
    web_socket_clients: WebSocketClients,
    webrtc_clients: WebRTCClients,
    addr: SocketAddr,
    mut ws_receiver: SplitStream<WebSocketStream<TcpStream>>,
) {
    while let Some(message) = ws_receiver.next().await {
        match message {
            Ok(message) => match message {
                Message::Text(text) => {
                    if let Ok(json) = serde_json::from_str::<Value>(&text) {
                        if let Some(msg_type) = json
                            .get("msg_type")
                            .map(|v| serde_json::from_value::<MessageType>(v.clone()))
                        {
                            match msg_type {
                                Ok(msg_type) => match msg_type {
                                    MessageType::Chat => {
                                        process_chat_message(
                                            json,
                                            web_socket_clients.clone(),
                                            &addr,
                                        )
                                        .await;
                                    }
                                    MessageType::Offer => {
                                        process_offer_message(
                                            json,
                                            web_socket_clients.clone(),
                                            webrtc_clients.clone(),
                                            &addr,
                                        )
                                        .await;
                                    }
                                    MessageType::Answer => {
                                        process_answer_message(json, webrtc_clients.clone(), &addr)
                                            .await;
                                    }
                                    MessageType::IceCandidate => {
                                        process_icecandidate_message(
                                            json,
                                            webrtc_clients.clone(),
                                            &addr,
                                        )
                                        .await;
                                    }
                                    MessageType::NewVideoTrack => {
                                        println!("NewVideoTrack");
                                    }
                                },
                                _ => {
                                    println!("Bilinmeyen msg_type: {:?}", msg_type);
                                }
                            }
                        }
                    }
                }
                Message::Close(_) => {
                    println!("{} bağlantıyı kapatıyor", addr);
                    break;
                }
                _ => {}
            },
            Err(e) => {
                println!("Error receiving message from {}: {}", addr, e);
                break;
            }
        }
    }
}

async fn process_chat_message(
    json: Value,
    web_socket_clients: WebSocketClients,
    addr: &SocketAddr,
) {
    if let Ok(chat_message) = serde_json::from_value::<ChatMessage>(json) {
        if let Ok(content) = serde_json::to_string(&chat_message) {
            let message = Message::Text(content);
            for client in web_socket_clients.read().await.iter() {
                if (*client.0) == *addr {
                    continue;
                }
                let _ = client.1.lock().await.send(message.clone());
            }
        }
    }
}

async fn process_offer_message(
    json: Value,
    web_socket_clients: WebSocketClients,
    webrtc_clients: WebRTCClients,
    addr: &SocketAddr,
) {
    if let Ok(offer_message) = serde_json::from_value::<OfferSignalMessage>(json) {
        let remote_sdp_string = offer_message.content;
        let peer_connection: Arc<Mutex<RTCPeerConnection>>;

        let webrtc_clients_read = webrtc_clients.read().await;
        if let Some(webrtc_client) = webrtc_clients_read.get(addr) {
            peer_connection = webrtc_client.clone();
        } else {
            drop(webrtc_clients_read);
            peer_connection = create_peer_connection().await;

            let video_track = Arc::new(
                webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP::new(
                    webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability {
                        mime_type: "video/vp8".to_owned(),
                        clock_rate: 90000,
                        channels: 0,
                        ..Default::default()
                    },
                    "video_01".to_owned(),
                    "video_stream_01".to_owned(),
                ),
            );

            /*let audio_track = Arc::new(TrackLocalStaticRTP::new(
                RTCRtpCodecCapability {
                    mime_type: "audio/opus".to_owned(),
                    clock_rate: 48000,
                    channels: 2,
                    ..Default::default()
                },
                "audio_01".to_owned(),
                "audio_stream_01".to_owned(),
            ));*/

            peer_connection
                .lock()
                .await
                .add_track(video_track.clone())
                .await
                .unwrap();

            gather_and_send_candidates(
                peer_connection.clone(),
                web_socket_clients.read().await.get(addr).unwrap().clone(),
            )
            .await;

            webrtc_clients
                .write()
                .await
                .insert(*addr, peer_connection.clone());

            on_track(peer_connection.clone(), video_track).await;
        }

        let peer_connection_locked = peer_connection.lock().await;

        peer_connection_locked
            .set_remote_description(RTCSessionDescription::offer(remote_sdp_string).unwrap())
            .await
            .unwrap();

        let answer = peer_connection_locked.create_answer(None).await.unwrap();
        peer_connection_locked
            .set_local_description(answer)
            .await
            .unwrap();

        let _ = web_socket_clients
            .read()
            .await
            .get(addr)
            .unwrap()
            .lock()
            .await
            .send(Message::Text(
                serde_json::to_string(&AnswerSignalMessage {
                    msg_type: "Answer".to_string(),
                    content: peer_connection_locked
                        .local_description()
                        .await
                        .unwrap()
                        .sdp,
                })
                .unwrap(),
            ));

        //tokio::spawn(deneme(peer_connection.clone()));
    }
}

async fn send_offer(
    peer_connection: Arc<Mutex<RTCPeerConnection>>,
    web_socket_clients: WebSocketClients,
    addr: &SocketAddr,
) {
    let offer = peer_connection
        .lock()
        .await
        .create_offer(None)
        .await
        .unwrap();
    peer_connection
        .lock()
        .await
        .set_local_description(offer)
        .await
        .unwrap();
    let offer_sdp = peer_connection.lock().await.local_description().await.unwrap().sdp;
    let offer_msg = OfferSignalMessage {
        msg_type: "Offer".to_string(),
        content: offer_sdp,
    };
    let json_msg = serde_json::to_string(&offer_msg).unwrap();
}

async fn process_answer_message(json: Value, webrtc_clients: WebRTCClients, addr: &SocketAddr) {
    if let Ok(answer_message) = serde_json::from_value::<AnswerSignalMessage>(json) {
        let remote_sdp_string = answer_message.content;
        let peer_connection: Arc<Mutex<RTCPeerConnection>>;

        let webrtc_clients_read = webrtc_clients.read().await;
        if let Some(webrtc_client) = webrtc_clients_read.get(addr) {
            peer_connection = webrtc_client.clone();
        } else {
            return;
        }

        let peer_connection_locked = peer_connection.lock().await;

        peer_connection_locked
            .set_remote_description(RTCSessionDescription::answer(remote_sdp_string).unwrap())
            .await
            .unwrap();
    }
}

async fn deneme(pc: Arc<Mutex<RTCPeerConnection>>) {
    let three_secs = core::time::Duration::from_secs(3);
    std::thread::sleep(three_secs);
    let video_track = Arc::new(
        webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP::new(
            webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability {
                mime_type: "video/vp8".to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: "".to_owned(),
                rtcp_feedback: vec![],
            },
            "video_01".to_owned(),
            "video_stream_01".to_owned(),
        ),
    );
    let audio_track = Arc::new(
        webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP::new(
            webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability {
                mime_type: "audio/opus".to_owned(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "".to_owned(),
                rtcp_feedback: vec![],
            },
            "audio_01".to_owned(),
            "audio_stream_01".to_owned(),
        ),
    );
    pc.lock().await.add_track(audio_track).await.unwrap();
    pc.lock().await.add_track(video_track).await.unwrap();
    println!("Track added");
}

async fn process_icecandidate_message(
    json: Value,
    webrtc_clients: WebRTCClients,
    addr: &SocketAddr,
) {
    if let Ok(icecandidate_message) = serde_json::from_value::<IceCandidateSignalMessage>(json) {
        let peer_connection: Arc<Mutex<RTCPeerConnection>>;

        let webrtc_clients_read = webrtc_clients.read().await;
        if let Some(webrtc_client) = webrtc_clients_read.get(addr) {
            peer_connection = webrtc_client.clone();
        } else {
            return;
        }

        let peer_connection_locked = peer_connection.lock().await;

        let candidate = RTCIceCandidateInit {
            candidate: icecandidate_message.content,
            sdp_mid: icecandidate_message.sdp_mid,
            sdp_mline_index: icecandidate_message.sdp_mline_index,
            username_fragment: icecandidate_message.username_fragment,
        };

        peer_connection_locked
            .add_ice_candidate(candidate)
            .await
            .unwrap();
    }
}

#[derive(Serialize, Deserialize, Debug)]
enum MessageType {
    Chat,
    Offer,
    Answer,
    IceCandidate,
    NewVideoTrack,
}

#[derive(Serialize, Deserialize, Debug)]
struct ChatMessage {
    msg_type: String,
    content: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct OfferSignalMessage {
    msg_type: String,
    content: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct AnswerSignalMessage {
    msg_type: String,
    content: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct IceCandidateSignalMessage {
    msg_type: String,
    content: String,
    sdp_mid: Option<String>,
    sdp_mline_index: Option<u16>,
    username_fragment: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct NewTrackSignalMessage {
    msg_type: String,
    ssrc: String,
    mid: String,
    payload_type: String,
}

async fn create_peer_connection() -> Arc<Mutex<RTCPeerConnection>> {
    let mut media_engine = MediaEngine::default();

    media_engine.register_default_codecs().unwrap();

    let mut registry = Registry::new();

    registry =
        interceptor_registry::register_default_interceptors(registry, &mut media_engine).unwrap();

    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build();

    let stun_server_1 = RTCIceServer {
        urls: vec!["stun:stun.cloudflare.com:3478".to_string()],
        ..Default::default()
    };

    let stun_server_2 = RTCIceServer {
        urls: vec!["stun:stun.l.google.com:19302".to_string()],
        ..Default::default()
    };

    let config = RTCConfiguration {
        ice_servers: vec![stun_server_1, stun_server_2],
        ice_transport_policy: RTCIceTransportPolicy::All,
        ..Default::default()
    };

    let peer_connection = api.new_peer_connection(config).await.unwrap();

    Arc::new(Mutex::new(peer_connection))
}

async fn add_tranceiever(webrtc_clients: WebRTCClients, addr: &SocketAddr) {
    let webrtc_clients_read = webrtc_clients.read().await;
    if let Some(webrtc_client) = webrtc_clients_read.get(addr) {
        let peer_connection_locked = webrtc_client.lock().await;
        peer_connection_locked
            .add_transceiver_from_kind(
                RTPCodecType::Video,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Sendonly,
                    send_encodings: vec![RTCRtpCodingParameters {
                        ssrc: 11111111,
                        ..Default::default()
                    }],
                }),
            )
            .await
            .unwrap();
    }
}

async fn gather_and_send_candidates(
    peer_connection: Arc<Mutex<RTCPeerConnection>>,
    ws_sender: Arc<Mutex<UnboundedSender<Message>>>,
) {
    peer_connection.lock().await.on_ice_candidate(Box::new(
        move |candidate: Option<RTCIceCandidate>| {
            let ws_sender = ws_sender.clone();
            Box::pin(async move {
                if let Some(candidate) = candidate {
                    let icecandidate_msg = IceCandidateSignalMessage {
                        msg_type: "IceCandidate".to_string(),
                        content: candidate.to_json().unwrap().candidate,
                        sdp_mid: candidate.to_json().unwrap().sdp_mid,
                        sdp_mline_index: candidate.to_json().unwrap().sdp_mline_index,
                        username_fragment: candidate.to_json().unwrap().username_fragment,
                    };
                    let json_msg = serde_json::to_string(&icecandidate_msg).unwrap();
                    ws_sender
                        .lock()
                        .await
                        .send(Message::Text(json_msg))
                        .unwrap();
                }
            })
        },
    ));
}

async fn on_trackk(
    peer_connection: Arc<Mutex<RTCPeerConnection>>,
    web_socket_clients: WebSocketClients,
    addr: SocketAddr,
) {
    peer_connection
        .lock()
        .await
        .on_track(Box::new(move |track: Arc<TrackRemote>, _, _| {
            let track = track.clone();
            let web_socket_clients = web_socket_clients.clone();
            Box::pin(async move {
                for client in web_socket_clients.write().await.iter_mut() {
                    if (*client.0) == addr {
                        client
                            .1
                            .lock()
                            .await
                            .send(Message::Text(
                                serde_json::to_string(&NewTrackSignalMessage {
                                    msg_type: "NewVideoTrack".to_string(),
                                    ssrc: track.ssrc().to_string(),
                                    mid: track.msid().to_string(),
                                    payload_type: track.payload_type().to_string(),
                                })
                                .unwrap(),
                            ))
                            .unwrap();
                    }
                }
            })
        }));
}

async fn on_track(
    peer_connection: Arc<Mutex<RTCPeerConnection>>,
    video_track: Arc<TrackLocalStaticRTP>,
) {
    peer_connection
        .lock()
        .await
        .on_track(Box::new(move |track: Arc<TrackRemote>, _, _| {
            let track = track.clone();
            let video_track = video_track.clone();
            Box::pin(async move {
                while let Ok((rtp, _)) = track.read_rtp().await {
                    if let Err(e) = video_track.write_rtp(&rtp).await {
                        eprintln!("Error writing RTP packet: {}", e);
                        break;
                    }
                }
            })
        }));
}
