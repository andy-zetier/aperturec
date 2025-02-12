//! QUIC client & server

use anyhow::{bail, Result};
#[cfg(target_os = "linux")]
use aperturec_utils::versioning;

mod client;
mod server;

pub use client::{AsyncClient, Builder as ClientBuilder, Client};
pub use server::{AsyncServer, Builder as ServerBuilder, Server};

/// Default port the server will bind to
pub const DEFAULT_SERVER_BIND_PORT: u16 = 46452;
/// Default port the client will bind to.
///
/// Since the value is 0, this will bind to a port of the OS's choosing
pub const DEFAULT_CLIENT_BIND_PORT: u16 = 0;
/// Variable for setting where the SSL key-logging file is saved to
pub const SSLKEYLOGFILE_VAR: &str = "SSLKEYLOGFILE";

#[cfg(target_os = "linux")]
pub(crate) fn validate_current_kernel_version() -> Result<()> {
    let kernel_version = versioning::running_kernel()?;
    validate_kernel_version(&kernel_version)
}

#[cfg(target_os = "linux")]
fn validate_kernel_version(kv: &versioning::Kernel) -> Result<()> {
    // Minimum Linux kernel version supported for GSO and GRO. Prior LTS versions of the kernel do
    // not support these features, and silently fail when attempting to use them.
    const MINIMUM_KERNEL_VERSION: &str = "4.18";
    if !kv.meets_or_exceeds(MINIMUM_KERNEL_VERSION)? {
        bail!(
            "kernel version {} is below minimum supported version {}",
            kv,
            MINIMUM_KERNEL_VERSION
        );
    }

    Ok(())
}

#[cfg(test)]
pub mod test {
    use super::*;
    use crate::*;

    use aperturec_protocol::*;

    use test_log::test;
    use tokio::runtime::Runtime as TokioRuntime;

    fn server_builder() -> server::Builder {
        server::Builder::default()
            .bind_addr("127.0.0.1:0")
            .tls_pem_certificate(&tls::test_material::PEM.certificate)
            .tls_pem_private_key(&tls::test_material::PEM.pkey)
    }

    fn client_builder() -> client::Builder {
        client::Builder::default()
            .additional_tls_pem_certificate(&tls::test_material::PEM.certificate)
    }

    fn unconnected_sync_client_sync_server() -> (Client, Server) {
        let s = server_builder().build_sync().expect("server build");
        let c = client_builder().build_sync().expect("client build");
        (c, s)
    }

    fn unconnected_async_client_async_server() -> (AsyncClient, AsyncServer) {
        let s = server_builder().build_async().expect("server build");
        let c = client_builder().build_async().expect("client build");
        (c, s)
    }

    fn unconnected_sync_client_async_server(rt: &TokioRuntime) -> (Client, AsyncServer) {
        let s = {
            let _guard = rt.enter();
            server_builder().build_async().expect("server build")
        };
        let c = client_builder().build_sync().expect("client build");
        (c, s)
    }

    fn unconnected_async_client_sync_server(rt: &TokioRuntime) -> (AsyncClient, Server) {
        let s = server_builder().build_sync().expect("server build");
        let c = {
            let _guard = rt.enter();
            client_builder().build_async().expect("client build")
        };
        (c, s)
    }

    fn test_messages() -> (
        (control::client_to_server::Message, usize),
        (control::server_to_client::Message, usize),
        (event::client_to_server::Message, usize),
        media::ServerToClient,
        tunnel::Message,
    ) {
        let ci: control::client_to_server::Message = control::ClientInit::default().into();
        let si: control::server_to_client::Message = control::ServerInit::default().into();
        let ke: event::client_to_server::Message = event::KeyEvent::default().into();
        let frag = media::ServerToClient {
            message: Some(media::FrameFragment::default().into()),
        };
        let ci_len = ci.encoded_len();
        let si_len = si.encoded_len();
        let ke_len = ke.encoded_len();
        let to = tunnel::Message {
            tunnel_id: 1,
            stream_id: 2,
            message: Some(tunnel::OpenTcpStream::new().into()),
        };
        (
            (ci, ci_len + prost::length_delimiter_len(ci_len)),
            (si, si_len + prost::length_delimiter_len(si_len)),
            (ke, ke_len + prost::length_delimiter_len(ke_len)),
            frag,
            to,
        )
    }

    #[test]
    fn sync_client_sync_server() {
        let (mut c, mut s) = unconnected_sync_client_sync_server();
        let (c_msgs, s_msgs) = (test_messages(), test_messages());
        let server_socket_addr = s.local_addr().expect("server addr");

        let s_thread = std::thread::spawn(move || {
            let session = s.accept().expect("server accept");
            let (mut cc, mut ec, mut mc, mut tc) = session.split();

            assert_eq!(cc.receive_with_len().expect("server cc receive"), s_msgs.0);
            cc.send(s_msgs.1 .0).expect("server cc send");
            assert_eq!(ec.receive_with_len().expect("server ec receive"), s_msgs.2);
            mc.send(s_msgs.3).expect("server mc send");
            tc.send(s_msgs.4.clone()).expect("server tc send");
            assert_eq!(tc.receive().expect("server tc receive"), s_msgs.4);

            session::Server::unsplit(cc, ec, mc, tc)
        });

        let c_thread = std::thread::spawn(move || {
            let session = c
                .connect(
                    &server_socket_addr.ip().to_string(),
                    server_socket_addr.port(),
                )
                .expect("client connect");
            let (mut cc, mut ec, mut mc, mut tc) = session.split();

            cc.send(c_msgs.0 .0).expect("client cc send");
            assert_eq!(cc.receive_with_len().expect("client cc receive"), c_msgs.1);
            ec.send(c_msgs.2 .0).expect("client ec send");
            assert_eq!(mc.receive().expect("client mc receive"), c_msgs.3);
            assert_eq!(tc.receive().expect("client tc receive"), c_msgs.4);
            tc.send(c_msgs.4).expect("client tc send");

            session::Client::unsplit(cc, ec, mc, tc)
        });

        let _s = s_thread.join().expect("server thread");
        let _c = c_thread.join().expect("client thread");
    }

    #[test(tokio::test)]
    async fn async_client_async_server() {
        let (mut c, mut s) = unconnected_async_client_async_server();
        let (c_msgs, s_msgs) = (test_messages(), test_messages());
        let server_socket_addr = s.local_addr().expect("server addr");

        let s_task = tokio::spawn(async move {
            let session = s.accept().await.expect("server accept");
            let (mut cc, mut ec, mut mc, mut tc) = session.split();

            assert_eq!(
                cc.receive_with_len().await.expect("server cc receive"),
                s_msgs.0
            );
            cc.send(s_msgs.1 .0).await.expect("server cc send");
            assert_eq!(
                ec.receive_with_len().await.expect("server ec receive"),
                s_msgs.2
            );
            mc.send(s_msgs.3).await.expect("server mc send");
            tc.send(s_msgs.4.clone()).await.expect("server tc send");
            tc.flush().await.expect("server tc flush");
            assert_eq!(tc.receive().await.expect("server tc receive"), s_msgs.4);

            session::AsyncServer::unsplit(cc, ec, mc, tc)
        });

        let c_task = tokio::spawn(async move {
            let session = c
                .connect(
                    &server_socket_addr.ip().to_string(),
                    server_socket_addr.port(),
                )
                .await
                .expect("client connect");
            let (mut cc, mut ec, mut mc, mut tc) = session.split();

            cc.send(c_msgs.0 .0).await.expect("client cc send");
            assert_eq!(
                cc.receive_with_len().await.expect("client cc receive"),
                c_msgs.1
            );
            ec.send(c_msgs.2 .0).await.expect("client ec send");
            assert_eq!(mc.receive().await.expect("client mc receive"), c_msgs.3);
            assert_eq!(tc.receive().await.expect("client tc receive"), c_msgs.4);
            tc.send(c_msgs.4).await.expect("client tc send");

            session::AsyncClient::unsplit(cc, ec, mc, tc)
        });

        let _c = c_task.await.expect("client task");
        let _s = s_task.await.expect("server task");
    }

    #[test]
    fn sync_client_async_server() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("rt");

        let (mut c, mut s) = unconnected_sync_client_async_server(&rt);
        let (c_msgs, s_msgs) = (test_messages(), test_messages());
        let server_socket_addr = s.local_addr().expect("server addr");

        let c_thread = std::thread::spawn(move || {
            let session = c
                .connect(
                    &server_socket_addr.ip().to_string(),
                    server_socket_addr.port(),
                )
                .expect("client connect");
            let (mut cc, mut ec, mut mc, mut tc) = session.split();

            cc.send(c_msgs.0 .0).expect("client cc send");
            assert_eq!(cc.receive_with_len().expect("client cc receive"), c_msgs.1);
            ec.send(c_msgs.2 .0).expect("client ec send");
            assert_eq!(mc.receive().expect("client mc receive"), c_msgs.3);
            assert_eq!(tc.receive().expect("client tc receive"), c_msgs.4);
            tc.send(c_msgs.4).expect("client tc send");

            session::Client::unsplit(cc, ec, mc, tc)
        });

        let _s = rt.block_on(async {
            let session = s.accept().await.expect("server accept");
            let (mut cc, mut ec, mut mc, mut tc) = session.split();

            assert_eq!(
                cc.receive_with_len().await.expect("server cc receive"),
                s_msgs.0
            );
            cc.send(s_msgs.1 .0).await.expect("server cc send");
            assert_eq!(
                ec.receive_with_len().await.expect("server ec receive"),
                s_msgs.2
            );
            mc.send(s_msgs.3).await.expect("server mc send");
            tc.send(s_msgs.4.clone()).await.expect("server tc send");
            assert_eq!(tc.receive().await.expect("server tc receive"), s_msgs.4);

            session::AsyncServer::unsplit(cc, ec, mc, tc)
        });

        let _c = c_thread.join().expect("client thread");
    }

    #[test]
    fn async_client_sync_server() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("rt");

        let (mut c, mut s) = unconnected_async_client_sync_server(&rt);
        let (c_msgs, s_msgs) = (test_messages(), test_messages());
        let server_socket_addr = s.local_addr().expect("server addr");

        let s_thread = std::thread::spawn(move || {
            let session = s.accept().expect("server accept");
            let (mut cc, mut ec, mut mc, mut tc) = session.split();

            assert_eq!(cc.receive_with_len().expect("server cc receive"), s_msgs.0);
            cc.send(s_msgs.1 .0).expect("server cc send");
            assert_eq!(ec.receive_with_len().expect("server ec receive"), s_msgs.2);
            mc.send(s_msgs.3).expect("server mc send");
            tc.send(s_msgs.4.clone()).expect("server tc send");
            assert_eq!(tc.receive().expect("server tc receive"), s_msgs.4);

            session::Server::unsplit(cc, ec, mc, tc)
        });

        let _c = rt.block_on(async {
            let session = c
                .connect(
                    &server_socket_addr.ip().to_string(),
                    server_socket_addr.port(),
                )
                .await
                .expect("client connect");
            let (mut cc, mut ec, mut mc, mut tc) = session.split();

            cc.send(c_msgs.0 .0).await.expect("client cc send");
            assert_eq!(
                cc.receive_with_len().await.expect("client cc receive"),
                c_msgs.1
            );
            ec.send(c_msgs.2 .0).await.expect("client ec send");
            assert_eq!(mc.receive().await.expect("client mc receive"), c_msgs.3);
            assert_eq!(tc.receive().await.expect("client tc receive"), c_msgs.4);
            tc.send(c_msgs.4).await.expect("client tc send");

            session::AsyncClient::unsplit(cc, ec, mc, tc)
        });

        let _s = s_thread.join().expect("server thread");
    }

    const PUSH_MEDIA_NUM_MESSAGES: usize = 4096;
    const PUSH_MEDIA_FRAGMENT_SIZE: usize = 4096;

    #[test]
    fn sync_client_sync_server_push_media() {
        let (mut c, mut s) = unconnected_sync_client_sync_server();
        let server_socket_addr = s.local_addr().expect("server addr");

        let s_thread = std::thread::spawn(move || {
            let session = s.accept().expect("server accept");
            let (_, _, mut mc, _) = session.split();

            for _ in 0..PUSH_MEDIA_NUM_MESSAGES {
                let frag = media::ServerToClient {
                    message: Some(
                        media::FrameFragment {
                            data: (0..PUSH_MEDIA_FRAGMENT_SIZE)
                                .map(|_| rand::random::<u8>())
                                .collect(),
                            ..Default::default()
                        }
                        .into(),
                    ),
                };
                mc.send(frag).expect("send");
            }
            mc
        });

        let c_thread = std::thread::spawn(move || {
            let session = c
                .connect(
                    &server_socket_addr.ip().to_string(),
                    server_socket_addr.port(),
                )
                .expect("client connect");
            let (_, _, mut mc, _) = session.split();

            for _ in 0..PUSH_MEDIA_NUM_MESSAGES {
                let frag_message = mc.receive().expect("receive").message.expect("message");
                if let media::server_to_client::Message::Fragment(frag) = frag_message {
                    assert_eq!(frag.data.len(), PUSH_MEDIA_FRAGMENT_SIZE);
                } else {
                    panic!("unexpected message");
                }
            }
            mc
        });

        let _c = c_thread.join().expect("client thread");
        let _s = s_thread.join().expect("server thread");
    }

    #[test(tokio::test)]
    async fn async_client_async_server_push_media() {
        let (mut c, mut s) = unconnected_async_client_async_server();
        let server_socket_addr = s.local_addr().expect("server addr");

        let s_task = tokio::spawn(async move {
            let session = s.accept().await.expect("server accept");
            let (_, _, mut mc, _) = session.split();

            for _ in 0..PUSH_MEDIA_NUM_MESSAGES {
                let frag = media::ServerToClient {
                    message: Some(
                        media::FrameFragment {
                            data: (0..PUSH_MEDIA_FRAGMENT_SIZE)
                                .map(|_| rand::random::<u8>())
                                .collect(),
                            ..Default::default()
                        }
                        .into(),
                    ),
                };
                mc.send(frag).await.expect("send");
            }
            mc
        });

        let c_task = tokio::spawn(async move {
            let session = c
                .connect(
                    &server_socket_addr.ip().to_string(),
                    server_socket_addr.port(),
                )
                .await
                .expect("client connect");
            let (_, _, mut mc, _) = session.split();

            for _ in 0..PUSH_MEDIA_NUM_MESSAGES {
                let frag_message = mc
                    .receive()
                    .await
                    .expect("receive")
                    .message
                    .expect("message");
                if let media::server_to_client::Message::Fragment(frag) = frag_message {
                    assert_eq!(frag.data.len(), PUSH_MEDIA_FRAGMENT_SIZE);
                } else {
                    panic!("unexpected message");
                }
            }
            mc
        });

        let _c = c_task.await.expect("client task");
        let _s = s_task.await.expect("server task");
    }

    #[test]
    fn sync_client_async_server_push_media() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("rt");

        let (mut c, mut s) = unconnected_sync_client_async_server(&rt);
        let server_socket_addr = s.local_addr().expect("server addr");

        let c_thread = std::thread::spawn(move || {
            let session = c
                .connect(
                    &server_socket_addr.ip().to_string(),
                    server_socket_addr.port(),
                )
                .expect("client connect");
            let (_, _, mut mc, _) = session.split();

            for _ in 0..PUSH_MEDIA_NUM_MESSAGES {
                let frag_message = mc.receive().expect("receive").message.expect("message");
                if let media::server_to_client::Message::Fragment(frag) = frag_message {
                    assert_eq!(frag.data.len(), PUSH_MEDIA_FRAGMENT_SIZE);
                } else {
                    panic!("unexpected message");
                }
            }
            mc
        });

        let _s = rt.block_on(async move {
            let session = s.accept().await.expect("server accept");
            let (_, _, mut mc, _) = session.split();

            for _ in 0..PUSH_MEDIA_NUM_MESSAGES {
                let frag = media::ServerToClient {
                    message: Some(
                        media::FrameFragment {
                            data: (0..PUSH_MEDIA_FRAGMENT_SIZE)
                                .map(|_| rand::random::<u8>())
                                .collect(),
                            ..Default::default()
                        }
                        .into(),
                    ),
                };
                mc.send(frag).await.expect("send");
            }
            mc
        });

        let _c = c_thread.join().expect("client thread");
    }

    #[test]
    fn async_client_sync_server_push_media() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("rt");

        let (mut c, mut s) = unconnected_async_client_sync_server(&rt);
        let server_socket_addr = s.local_addr().expect("server addr");

        let s_thread = std::thread::spawn(move || {
            let session = s.accept().expect("server accept");
            let (_, _, mut mc, _) = session.split();

            for _ in 0..PUSH_MEDIA_NUM_MESSAGES {
                let frag = media::ServerToClient {
                    message: Some(
                        media::FrameFragment {
                            data: (0..PUSH_MEDIA_FRAGMENT_SIZE)
                                .map(|_| rand::random::<u8>())
                                .collect(),
                            ..Default::default()
                        }
                        .into(),
                    ),
                };
                mc.send(frag).expect("send");
            }
            mc
        });

        let _c = rt.block_on(async {
            let session = c
                .connect(
                    &server_socket_addr.ip().to_string(),
                    server_socket_addr.port(),
                )
                .await
                .expect("client connect");
            let (_, _, mut mc, _) = session.split();

            for _ in 0..PUSH_MEDIA_NUM_MESSAGES {
                let frag_message = mc
                    .receive()
                    .await
                    .expect("receive")
                    .message
                    .expect("message");
                if let media::server_to_client::Message::Fragment(frag) = frag_message {
                    assert_eq!(frag.data.len(), PUSH_MEDIA_FRAGMENT_SIZE);
                } else {
                    panic!("unexpected message");
                }
            }
            mc
        });

        let _s = s_thread.join().expect("server thread");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn valid_kernel_version() {
        let kv = versioning::Kernel::try_from("4.18").expect("create kernel version");
        super::validate_kernel_version(&kv).expect("validate kernel version");
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[should_panic]
    fn invalid_kernel_version() {
        let kv = versioning::Kernel::try_from("4.17").expect("create kernel version");
        super::validate_kernel_version(&kv).expect("validate kernel version");
    }
}
