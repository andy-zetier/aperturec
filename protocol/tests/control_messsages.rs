extern crate aperturec_protocol;
extern crate asn1rs;

use aperturec_protocol::common_types::*;
use aperturec_protocol::control_messages::*;
use asn1rs::prelude::*;

#[macro_use]
mod macros;

#[test]
fn serde_client_info() {
    serde_type!(
        ClientInfo,
        ClientInfo {
            version: SemVer {
                major: 0,
                minor: 1,
                patch: 2
            },
            build_id: "asdf".into(),
            os: Os::Linux,
            os_version: "Bionic Beaver".into(),
            ssl_library: "OpenSSL".into(),
            ssl_version: "1.2".into(),
            bitness: Bitness::B32,
            endianness: Endianness::Little,
            architecture: Architecture::X86,
            cpu_id: "Haswell".into(),
            number_of_cores: 4,
            amount_of_ram: "2.4Gb".into(),
            display_size: Dimension {
                height: 1024,
                width: 768
            },
        }
    );
}

#[test]
fn serde_client_init() {
    serde_type!(
        ClientInit,
        ClientInit {
            temp_id: ClientId(1234),
            client_info: ClientInfo {
                version: SemVer {
                    major: 0,
                    minor: 1,
                    patch: 2
                },
                build_id: "asdf".into(),
                os: Os::Linux,
                os_version: "Bionic Beaver".into(),
                ssl_library: "OpenSSL".into(),
                ssl_version: "1.2".into(),
                bitness: Bitness::B64,
                endianness: Endianness::Big,
                architecture: Architecture::X86,
                cpu_id: "Haswell".into(),
                number_of_cores: 4,
                amount_of_ram: "2.4Gb".into(),
                display_size: Dimension {
                    height: 1024,
                    width: 768
                },
            },
            client_caps: ClientCaps {
                supported_codecs: vec![Codec::Avif],
            },
            client_heartbeat_interval: DurationMs(1000),
            client_heartbeat_response_interval: DurationMs(1000),
        }
    );
}

#[test]
fn serde_client_goodbye() {
    serde_type!(
        ClientGoodbye,
        ClientGoodbye {
            client_id: ClientId(2345),
            reason: ClientGoodbyeReason::UserRequested
        }
    );
}

#[test]
fn serde_heartbeat_response() {
    serde_type!(
        HeartbeatResponse,
        HeartbeatResponse {
            heartbeat_id: HeartbeatId(890),
            last_sequence_id: SequenceId(10),
        }
    );
}

#[test]
fn serde_framebuffer_update_request() {
    serde_type!(
        FramebufferUpdateRequest,
        FramebufferUpdateRequest {
            location: Location {
                x_position: 0,
                y_position: 100
            },
            dimension: Dimension {
                width: 400,
                height: 2
            }
        }
    );
}

#[test]
fn serde_missed_frame_report() {
    serde_type!(
        MissedFrameReport,
        MissedFrameReport {
            sequence_ids: vec![SequenceId(1), SequenceId(2), SequenceId(3),],
        }
    );
}

#[test]
fn serde_client_to_server_message() {
    serde_type!(
        ClientToServerMessage,
        ClientToServerMessage::ClientGoodbye(ClientGoodbye {
            client_id: ClientId(1234),
            reason: ClientGoodbyeReason::Terminating,
        })
    );
}

#[test]
fn serde_server_init() {
    serde_type!(
        ServerInit,
        ServerInit {
            client_id: ClientId(567),
            server_name: String::from("Some sweet server")
        }
    );
}

#[test]
fn serde_server_goodbye() {
    serde_type!(
        ServerGoodbye,
        ServerGoodbye {
            reason: ServerGoodbyeReason::ServerShuttingDown
        }
    );
}

#[test]
fn serde_heartbeat_request() {
    serde_type!(
        HeartbeatRequest,
        HeartbeatRequest {
            request_id: HeartbeatId(1234),
        }
    );
}

#[test]
fn serde_server_to_client_message() {
    serde_type!(
        ServerToClientMessage,
        ServerToClientMessage::ServerGoodbye(ServerGoodbye {
            reason: ServerGoodbyeReason::ServerShuttingDown,
        })
    );
}
