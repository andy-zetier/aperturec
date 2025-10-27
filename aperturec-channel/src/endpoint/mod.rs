//! QUIC client & server
use crate::{tls, util};

#[cfg(target_os = "linux")]
use aperturec_utils::versioning;

use std::{convert::Infallible, io, net::AddrParseError};

mod client;
mod server;

pub use client::{AsyncClient, Builder as ClientBuilder, Client, ConnectError};
pub use server::{AcceptError, AsyncServer, Builder as ServerBuilder, Server};

/// Default port the server will bind to
pub const DEFAULT_SERVER_BIND_PORT: u16 = 46452;
/// Default port the client will bind to.
///
/// Since the value is 0, this will bind to a port of the OS's choosing
pub const DEFAULT_CLIENT_BIND_PORT: u16 = 0;
/// Variable for setting where the SSL key-logging file is saved to
pub const SSLKEYLOGFILE_VAR: &str = "SSLKEYLOGFILE";

/// Errors that occur when building QUIC client or server endpoints.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// Kernel version does not meet minimum requirement
    ///
    /// On Linux, QUIC performance features (GSO/GRO) require kernel 4.18+.
    /// This error occurs on older kernels that lack these features.
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    KernelVersion(#[from] KernelValidationError),

    /// Failed to create the async runtime.
    ///
    /// Occurs when building a synchronous endpoint and the tokio runtime
    /// cannot be created.
    #[error(transparent)]
    BuildRuntime(#[from] util::BuildRuntimeError),

    /// Attempted to build an async endpoint while already within a sync runtime context.
    ///
    /// Async endpoints must be built directly within an async context, not
    /// from a synchronous wrapper that creates its own runtime.
    #[error("building async endpoint within sync runtime")]
    AsyncInSync,

    /// Attempted to build a sync endpoint while within an async runtime context.
    ///
    /// Sync endpoints cannot be built from within an async runtime as they
    /// need to create their own dedicated runtime.
    #[error("building sync endpoint within async runtime")]
    SyncInAsync,

    /// TLS configuration error.
    ///
    /// Wraps errors from certificate loading, private key setup, or TLS
    /// provider configuration. See [`tls::Error`] for details.
    #[error(transparent)]
    Tls(#[from] tls::Error),

    /// IO error during endpoint setup.
    ///
    /// Typically occurs when binding to network addresses or accessing
    /// filesystem resources during configuration.
    #[error(transparent)]
    IO(#[from] io::Error),

    /// No bind address was specified.
    ///
    /// Server endpoints require a bind address to listen on. This error
    /// indicates the address was not provided during builder configuration.
    #[error("no bind address")]
    NoBindAddress,

    /// Failed to parse network address.
    ///
    /// The provided address string could not be parsed into a valid
    /// socket address (e.g., invalid IP address or port number).
    #[error(transparent)]
    ParseAddress(#[from] AddrParseError),

    /// QUIC provider failed to start.
    ///
    /// Wraps errors from s2n-quic when initializing the QUIC endpoint
    #[error(transparent)]
    Start(#[from] s2n_quic::provider::StartError),
}

impl From<Infallible> for BuildError {
    fn from(_: Infallible) -> Self {
        unreachable!("convert infallible to error");
    }
}

/// Errors that occur during Linux kernel version validation.
///
/// ApertureC requires Linux kernel 4.18 or newer for optimal QUIC performance
/// via Generic Segmentation Offload (GSO) and Generic Receive Offload (GRO).
/// Older kernels lack these features and will silently fail to achieve expected
/// performance characteristics.
///
/// This error type is only available on Linux systems.
#[cfg(target_os = "linux")]
#[derive(Debug, thiserror::Error)]
pub enum KernelValidationError {
    /// Failed to identify the running kernel version.
    ///
    /// This error occurs when the system kernel version cannot be determined,
    /// typically due to missing system files or unexpected version string format.
    #[error(transparent)]
    IdentifyRunningKernel(#[from] versioning::KernelError),

    /// Kernel version does not meet minimum requirements.
    ///
    /// The detected kernel version is older than the minimum required version (4.18).
    /// Upgrading to a newer kernel is required for proper operation.
    #[error("kernel version {discovered} does not meet minimum required kernel {minimum}")]
    RequirementNotMet { discovered: String, minimum: String },
}

#[cfg(target_os = "linux")]
pub(crate) fn validate_current_kernel_version() -> std::result::Result<(), KernelValidationError> {
    let kernel_version = versioning::running_kernel()?;
    validate_kernel_version(&kernel_version)
}

#[cfg(target_os = "linux")]
fn validate_kernel_version(
    kv: &versioning::Kernel,
) -> std::result::Result<(), KernelValidationError> {
    // Minimum Linux kernel version supported for GSO and GRO. Prior LTS versions of the kernel do
    // not support these features, and silently fail when attempting to use them.
    const MINIMUM_KERNEL_VERSION: &str = "4.18";
    if !kv.meets_or_exceeds(MINIMUM_KERNEL_VERSION)? {
        Err(KernelValidationError::RequirementNotMet {
            discovered: kv.to_string(),
            minimum: MINIMUM_KERNEL_VERSION.to_string(),
        })
    } else {
        Ok(())
    }
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

    type TestMessages = (
        (control::client_to_server::Message, usize),
        (control::server_to_client::Message, usize),
        (event::client_to_server::Message, usize),
        media::ServerToClient,
        tunnel::Message,
    );

    fn test_messages() -> TestMessages {
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
            cc.send(s_msgs.1.0).expect("server cc send");
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

            cc.send(c_msgs.0.0).expect("client cc send");
            assert_eq!(cc.receive_with_len().expect("client cc receive"), c_msgs.1);
            ec.send(c_msgs.2.0).expect("client ec send");
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
            cc.send(s_msgs.1.0).await.expect("server cc send");
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

            cc.send(c_msgs.0.0).await.expect("client cc send");
            assert_eq!(
                cc.receive_with_len().await.expect("client cc receive"),
                c_msgs.1
            );
            ec.send(c_msgs.2.0).await.expect("client ec send");
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

            cc.send(c_msgs.0.0).expect("client cc send");
            assert_eq!(cc.receive_with_len().expect("client cc receive"), c_msgs.1);
            ec.send(c_msgs.2.0).expect("client ec send");
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
            cc.send(s_msgs.1.0).await.expect("server cc send");
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
            cc.send(s_msgs.1.0).expect("server cc send");
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

            cc.send(c_msgs.0.0).await.expect("client cc send");
            assert_eq!(
                cc.receive_with_len().await.expect("client cc receive"),
                c_msgs.1
            );
            ec.send(c_msgs.2.0).await.expect("client ec send");
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
