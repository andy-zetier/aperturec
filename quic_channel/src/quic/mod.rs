//! QUIC client & server
pub mod client;
pub mod server;

/// Default port the server will bind to
pub const DEFAULT_SERVER_BIND_PORT: u16 = 46452;
/// Default port the client will bind to.
///
/// Since the value is 0, this will bind to a port of the OS's choosing
pub const DEFAULT_CLIENT_BIND_PORT: u16 = 0;
/// Variable for setting where the SSL key-logging file is saved to
pub const SSLKEYLOGFILE_VAR: &str = "SSLKEYLOGFILE";

pub use client::Client;
pub use server::Server;

#[cfg(test)]
pub mod test {
    use super::*;
    use crate::*;

    use aperturec_state_machine::*;

    use tokio::runtime::Runtime as TokioRuntime;

    fn server_builder() -> server::Builder {
        server::Builder::default()
            .bind_addr("127.0.0.1:0")
            .tls_pem_certificate(&tls::test_material::PEM.certificate)
            .tls_pem_private_key(&tls::test_material::PEM.pkey)
    }

    fn client_builder(server_port: u16) -> client::Builder {
        client::Builder::default()
            .server_addr("localhost")
            .server_port(server_port)
            .additional_tls_pem_certificate(&tls::test_material::PEM.certificate)
    }

    fn unconnected_sync_client_sync_server() -> (
        client::Client<client::states::Closed>,
        server::Server<server::states::Listening>,
    ) {
        let s = server_builder().build_sync().expect("server build");
        let c = client_builder(s.local_addr().expect("local addr").port())
            .build_sync()
            .expect("client build");
        (c, s)
    }

    fn unconnected_async_client_async_server() -> (
        client::Client<client::states::AsyncClosed>,
        server::Server<server::states::AsyncListening>,
    ) {
        let s = server_builder().build_async().expect("server build");
        let c = client_builder(s.local_addr().expect("local addr").port())
            .build_async()
            .expect("client build");
        (c, s)
    }

    fn unconnected_sync_client_async_server(
        rt: &TokioRuntime,
    ) -> (
        client::Client<client::states::Closed>,
        server::Server<server::states::AsyncListening>,
    ) {
        let s = {
            let _guard = rt.enter();
            server_builder().build_async().expect("server build")
        };
        let c = client_builder(s.local_addr().expect("local addr").port())
            .build_sync()
            .expect("client build");
        (c, s)
    }

    fn unconnected_async_client_sync_server(
        rt: &TokioRuntime,
    ) -> (
        client::Client<client::states::AsyncClosed>,
        server::Server<server::states::Listening>,
    ) {
        let s = server_builder().build_sync().expect("server build");
        let c = {
            let _guard = rt.enter();
            client_builder(s.local_addr().expect("local addr").port())
                .build_async()
                .expect("client build")
        };
        (c, s)
    }

    fn test_messages() -> (
        (control::client_to_server::Message, usize),
        (control::server_to_client::Message, usize),
        (event::client_to_server::Message, usize),
        (media::server_to_client::Message, usize),
    ) {
        let ci: control::client_to_server::Message = control::ClientInit::default().into();
        let si: control::server_to_client::Message = control::ServerInit::default().into();
        let ke: event::client_to_server::Message = event::KeyEvent::default().into();
        let fbu: media::server_to_client::Message = media::FramebufferUpdate::default().into();
        let ci_len = ci.encoded_len();
        let si_len = si.encoded_len();
        let ke_len = ke.encoded_len();
        let fbu_len = fbu.encoded_len();
        (
            (ci, ci_len + prost::length_delimiter_len(ci_len)),
            (si, si_len + prost::length_delimiter_len(si_len)),
            (ke, ke_len + prost::length_delimiter_len(ke_len)),
            (fbu, fbu_len),
        )
    }

    #[test]
    fn sync_client_sync_server() {
        let (c, s) = unconnected_sync_client_sync_server();
        let (c_msgs, s_msgs) = (test_messages(), test_messages());

        let s_thread = std::thread::spawn(move || {
            let s = try_transition!(s, server::states::Accepted).expect("server accept");
            let s = try_transition!(s, server::states::Ready).expect("server ready");
            let (mut cc, mut ec, mut mc, residual) = s.split();

            assert_eq!(cc.receive_with_len().expect("server cc receive"), s_msgs.0);
            cc.send(s_msgs.1 .0).expect("server cc send");
            let _ = ec.receive_with_len().expect("server ec receive");
            assert_eq!(ec.receive_with_len().expect("server ec receive"), s_msgs.2);
            mc.send(s_msgs.3 .0).expect("server mc send");

            <server::Server<_> as UnifiedServer>::unsplit(cc, ec, mc, residual)
        });

        let c_thread = std::thread::spawn(move || {
            let c = try_transition!(c, client::states::Connected)
                .map_err(|rec| rec.error)
                .expect("client connect");
            let c = try_transition!(c, client::states::Ready).expect("client ready");
            let (mut cc, mut ec, mut mc, _) = c.split();

            cc.send(c_msgs.0 .0).expect("client cc send");
            assert_eq!(cc.receive_with_len().expect("client cc receive"), c_msgs.1);
            ec.send(c_msgs.2 .0).expect("client ec send");
            assert_eq!(mc.receive_with_len().expect("client mc receive"), c_msgs.3);

            <client::Client<_> as UnifiedClient>::unsplit(cc, ec, mc, ())
        });

        let _s = s_thread.join().expect("server thread");
        let _c = c_thread.join().expect("client thread");
    }

    #[tokio::test]
    async fn async_client_and_server() {
        let (c, s) = unconnected_async_client_async_server();
        let (c_msgs, s_msgs) = (test_messages(), test_messages());

        let s_task = tokio::spawn(async move {
            let s = try_transition_async!(s, server::states::AsyncAccepted).expect("server accept");
            let s = try_transition_async!(s, server::states::AsyncReady).expect("server ready");
            let (mut cc, mut ec, mut mc, residual) = s.split();

            assert_eq!(
                cc.receive_with_len().await.expect("server cc receive"),
                s_msgs.0
            );
            cc.send(s_msgs.1 .0).await.expect("server cc send");
            let _ = ec.receive_with_len().await.expect("server ec receive");
            assert_eq!(
                ec.receive_with_len().await.expect("server ec receive"),
                s_msgs.2
            );
            mc.send(s_msgs.3 .0).await.expect("server mc send");

            <server::Server<_> as AsyncUnifiedServer>::unsplit(cc, ec, mc, residual)
        });

        let c_task = tokio::spawn(async move {
            let c = try_transition_async!(c, client::states::AsyncConnected)
                .map_err(|rec| rec.error)
                .expect("client connect");
            let c = try_transition_async!(c, client::states::AsyncReady).expect("client ready");
            let (mut cc, mut ec, mut mc) = c.split();

            cc.send(c_msgs.0 .0).await.expect("client cc send");
            assert_eq!(
                cc.receive_with_len().await.expect("client cc receive"),
                c_msgs.1
            );
            ec.send(c_msgs.2 .0).await.expect("client ec send");
            assert_eq!(
                mc.receive_with_len().await.expect("client mc receive"),
                c_msgs.3
            );

            <client::Client<_> as AsyncUnifiedClient>::unsplit(cc, ec, mc)
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

        let (c, s) = unconnected_sync_client_async_server(&rt);
        let (c_msgs, s_msgs) = (test_messages(), test_messages());

        let c_thread = std::thread::spawn(move || {
            let c = try_transition!(c, client::states::Connected)
                .map_err(|rec| rec.error)
                .expect("client connect");
            let c = try_transition!(c, client::states::Ready).expect("client ready");
            let (mut cc, mut ec, mut mc, _) = c.split();

            cc.send(c_msgs.0 .0).expect("client cc send");
            assert_eq!(cc.receive_with_len().expect("client cc receive"), c_msgs.1);
            ec.send(c_msgs.2 .0).expect("client ec send");
            assert_eq!(mc.receive_with_len().expect("client mc receive"), c_msgs.3);

            <client::Client<_> as UnifiedClient>::unsplit(cc, ec, mc, ())
        });

        let _s = rt.block_on(async {
            let s = try_transition_async!(s, server::states::AsyncAccepted).expect("server accept");
            let s = try_transition_async!(s, server::states::AsyncReady).expect("server ready");
            let (mut cc, mut ec, mut mc, residual) = s.split();

            assert_eq!(
                cc.receive_with_len().await.expect("server cc receive"),
                s_msgs.0
            );
            cc.send(s_msgs.1 .0).await.expect("server cc send");
            let _ = ec.receive_with_len().await.expect("server ec receive");
            assert_eq!(
                ec.receive_with_len().await.expect("server ec receive"),
                s_msgs.2
            );
            mc.send(s_msgs.3 .0).await.expect("server mc send");

            <server::Server<_> as AsyncUnifiedServer>::unsplit(cc, ec, mc, residual)
        });

        let _c = c_thread.join().expect("client thread");
    }

    #[test]
    fn async_client_sync_server() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("rt");

        let (c, s) = unconnected_async_client_sync_server(&rt);
        let (c_msgs, s_msgs) = (test_messages(), test_messages());

        let s_thread = std::thread::spawn(move || {
            let s = try_transition!(s, server::states::Accepted).expect("server accept");
            let s = try_transition!(s, server::states::Ready).expect("server ready");
            let (mut cc, mut ec, mut mc, residual) = s.split();

            assert_eq!(cc.receive_with_len().expect("server cc receive"), s_msgs.0);
            cc.send(s_msgs.1 .0).expect("server cc send");
            let _ = ec.receive_with_len().expect("server ec receive");
            assert_eq!(ec.receive_with_len().expect("server ec receive"), s_msgs.2);
            mc.send(s_msgs.3 .0).expect("server mc send");

            <server::Server<_> as UnifiedServer>::unsplit(cc, ec, mc, residual)
        });

        let _c = rt.block_on(async {
            let c = try_transition_async!(c, client::states::AsyncConnected)
                .map_err(|rec| rec.error)
                .expect("client connect");
            let c = try_transition_async!(c, client::states::AsyncReady).expect("client ready");
            let (mut cc, mut ec, mut mc) = c.split();

            cc.send(c_msgs.0 .0).await.expect("client cc send");
            assert_eq!(
                cc.receive_with_len().await.expect("client cc receive"),
                c_msgs.1
            );
            ec.send(c_msgs.2 .0).await.expect("client ec send");
            assert_eq!(
                mc.receive_with_len().await.expect("client mc receive"),
                c_msgs.3
            );

            <client::Client<_> as AsyncUnifiedClient>::unsplit(cc, ec, mc)
        });

        let _s = s_thread.join().expect("server thread");
    }
}
