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
        #[cfg(feature = "asn1c-tests")]
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
                supported_codecs: vec![Codec::Zlib],
            },
            client_heartbeat_interval: DurationMs(1000),
            client_heartbeat_response_interval: DurationMs(1000),
            max_decoder_count: 56,
        };
        serde_type_der!(ClientInit, client_init);
        #[cfg(feature = "asn1c-tests")]
        c::round_trip_der!(ClientInit, c::ClientInit, client_init);
    }

    #[test]
    fn client_goodbye() {
        let client_goodbye = ClientGoodbye {
            client_id: ClientId(2345),
            reason: ClientGoodbyeReason::UserRequested,
        };
        serde_type_der!(ClientGoodbye, client_goodbye);
        #[cfg(feature = "asn1c-tests")]
        c::round_trip_der!(ClientGoodbye, c::ClientGoodbye, client_goodbye);
    }

    #[test]
    fn heartbeat_response() {
        let hb_response = HeartbeatResponse {
            heartbeat_id: HeartbeatId(890),
            last_sequence_ids: vec![
                DecoderSequencePair::new(Decoder::new(5000), SequenceId::new(300)),
                DecoderSequencePair::new(Decoder::new(5001), SequenceId::new(301)),
            ],
        };
        serde_type_der!(HeartbeatResponse, hb_response);
        #[cfg(feature = "asn1c-tests")]
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
        #[cfg(feature = "asn1c-tests")]
        c::round_trip_der!(
            FramebufferUpdateRequest,
            c::FramebufferUpdateRequest,
            fb_update_req
        );
    }

    #[test]
    fn missed_frame_report() {
        let missed_frame_report = MissedFrameReport {
            decoder: Decoder::new(1234),
            frames: vec![SequenceId(1), SequenceId(2), SequenceId(3)],
        };
        serde_type_der!(MissedFrameReport, missed_frame_report);
        #[cfg(feature = "asn1c-tests")]
        c::round_trip_der!(MissedFrameReport, c::MissedFrameReport, missed_frame_report);
    }

    #[test]
    fn client_to_server_message() {
        let msg = ClientToServerMessage::ClientGoodbye(ClientGoodbye {
            client_id: ClientId(1234),
            reason: ClientGoodbyeReason::Terminating,
        });
        serde_type_der!(ClientToServerMessage, msg);
        #[cfg(feature = "asn1c-tests")]
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
        #[cfg(feature = "asn1c-tests")]
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
        #[cfg(feature = "asn1c-tests")]
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
        #[cfg(feature = "asn1c-tests")]
        c::round_trip_der!(ServerGoodbye, c::ServerGoodbye, goodbye);
    }

    #[test]
    fn heartbeat_request() {
        let hb_request = HeartbeatRequest {
            request_id: HeartbeatId(1234),
        };

        serde_type_der!(HeartbeatRequest, hb_request);
        #[cfg(feature = "asn1c-tests")]
        c::round_trip_der!(HeartbeatRequest, c::HeartbeatRequest, hb_request);
    }

    #[test]
    fn server_to_client_message_goodbye() {
        let msg = ServerToClientMessage::ServerGoodbye(ServerGoodbye {
            reason: ServerGoodbyeReason::NetworkError,
        });
        serde_type_der!(ServerToClientMessage, msg);
        #[cfg(feature = "asn1c-tests")]
        c::round_trip_der!(
            super::ServerToClientMessage,
            c::ControlMessages_ServerToClientMessage,
            msg
        )
    }
}
