include!(concat!(env!("OUT_DIR"), "/control_messages.rs"));

#[cfg(test)]
mod test {
    use crate::common_types::*;
    use crate::control_messages::*;
    use crate::test::*;

    #[test]
    fn client_info() {
        let client_info = ClientInfo {
            version: SemVer {
                major: 0,
                minor: 1,
                patch: 2,
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
                width: 768,
            },
        };
        serde_type_der!(ClientInfo, client_info);
        c::round_trip_der!(ClientInfo, c::ClientInfo, client_info);
    }

    #[test]
    fn client_init() {
        let client_init = ClientInit {
            temp_id: ClientId(1234),
            client_info: ClientInfo {
                version: SemVer {
                    major: 0,
                    minor: 1,
                    patch: 2,
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
                    width: 768,
                },
            },
            client_caps: ClientCaps {
                supported_codecs: vec![Codec::Avif],
            },
            client_heartbeat_interval: DurationMs(1000),
            client_heartbeat_response_interval: DurationMs(1000),
            decoders: vec![Decoder { port: 1024 }],
        };
        serde_type_der!(ClientInit, client_init);
        c::round_trip_der!(ClientInit, c::ClientInit, client_init);
    }

    #[test]
    fn client_goodbye() {
        let client_goodbye = ClientGoodbye {
            client_id: ClientId(2345),
            reason: ClientGoodbyeReason::UserRequested,
        };
        serde_type_der!(ClientGoodbye, client_goodbye);
        c::round_trip_der!(ClientGoodbye, c::ClientGoodbye, client_goodbye);
    }

    #[test]
    fn heartbeat_response() {
        let hb_response = HeartbeatResponse {
            heartbeat_id: HeartbeatId(890),
            last_sequence_id: SequenceId(10),
        };
        serde_type_der!(HeartbeatResponse, hb_response);
        c::round_trip_der!(HeartbeatResponse, c::HeartbeatResponse, hb_response);
    }

    #[test]
    fn framebuffer_update_request() {
        let fb_update_req = FramebufferUpdateRequest {
            location: Location {
                x_position: 0,
                y_position: 100,
            },
            dimension: Dimension {
                width: 400,
                height: 2,
            },
        };
        serde_type_der!(FramebufferUpdateRequest, fb_update_req);
        c::round_trip_der!(
            FramebufferUpdateRequest,
            c::FramebufferUpdateRequest,
            fb_update_req
        );
    }

    #[test]
    fn missed_frame_report() {
        serde_type_der!(
            MissedFrameReport,
            MissedFrameReport {
                sequence_ids: vec![SequenceId(1), SequenceId(2), SequenceId(3),],
            }
        );
    }

    #[test]
    fn client_to_server_message() {
        let msg = ClientToServerMessage::ClientGoodbye(ClientGoodbye {
            client_id: ClientId(1234),
            reason: ClientGoodbyeReason::Terminating,
        });
        serde_type_der!(ClientToServerMessage, msg);
        c::round_trip_der!(
            ClientToServerMessage,
            c::ControlMessages_ClientToServerMessage,
            msg
        );
    }

    #[test]
    fn server_init() {
        let server_init = ServerInit {
            client_id: ClientId(567),
            server_name: String::from("Some sweet server"),
            event_port: 12345,
            display_size: Dimension {
                width: 800,
                height: 600,
            },
            cursor_bitmaps: Some(vec![
                CursorBitmap {
                    cursor: Cursor::Default,
                    data: vec![110, 20, 30].into(),
                },
                CursorBitmap {
                    cursor: Cursor::Text,
                    data: vec![40, 50, 66].into(),
                },
            ]),
            decoder_areas: vec![DecoderArea {
                decoder: Decoder { port: 1234 },
                location: Location {
                    x_position: 10,
                    y_position: 20,
                },
                dimension: Dimension {
                    width: 50,
                    height: 50,
                },
            }],
        };
        serde_type_der!(ServerInit, server_init);
        c::round_trip_der!(ServerInit, c::ServerInit, server_init);
    }

    #[test]
    fn server_to_client_message_init() {
        let server_init = ServerInit {
            client_id: ClientId(567),
            server_name: String::from("Some sweet server"),
            event_port: 12345,
            display_size: Dimension {
                width: 800,
                height: 600,
            },
            cursor_bitmaps: Some(vec![
                CursorBitmap {
                    cursor: Cursor::Default,
                    data: vec![110, 20, 30].into(),
                },
                CursorBitmap {
                    cursor: Cursor::Text,
                    data: vec![40, 50, 66].into(),
                },
            ]),
            decoder_areas: vec![DecoderArea {
                decoder: Decoder { port: 1234 },
                location: Location {
                    x_position: 10,
                    y_position: 20,
                },
                dimension: Dimension {
                    width: 50,
                    height: 50,
                },
            }],
        };
        let msg = ServerToClientMessage::ServerInit(server_init);
        serde_type_der!(ServerToClientMessage, msg);
        c::round_trip_der!(
            ServerToClientMessage,
            c::ControlMessages_ServerToClientMessage,
            msg
        );
    }

    #[test]
    fn server_goodbye() {
        let goodbye = ServerGoodbye {
            reason: ServerGoodbyeReason::ServerShuttingDown,
        };
        serde_type_der!(ServerGoodbye, goodbye);
        c::round_trip_der!(ServerGoodbye, c::ServerGoodbye, goodbye);
    }

    #[test]
    fn heartbeat_request() {
        let hb_request = HeartbeatRequest {
            request_id: HeartbeatId(1234),
        };

        serde_type_der!(HeartbeatRequest, hb_request);
        c::round_trip_der!(HeartbeatRequest, c::HeartbeatRequest, hb_request);
    }

    #[test]
    fn server_to_client_message_goodbye() {
        let msg = ServerToClientMessage::ServerGoodbye(ServerGoodbye {
            reason: ServerGoodbyeReason::NetworkError,
        });
        serde_type_der!(ServerToClientMessage, msg);
        c::round_trip_der!(
            super::ServerToClientMessage,
            c::ControlMessages_ServerToClientMessage,
            msg
        )
    }
}
