use crate::server;
use aperturec_channel::{self as channel, AsyncReceiver, Unified};
use aperturec_protocol::control::{self as cm, client_to_server as cm_c2s};

use anyhow::Result;
use futures::{prelude::*, stream::BoxStream};
use pbkdf2::{
    password_hash::{PasswordHasher, PasswordVerifier, SaltString},
    Pbkdf2,
};
use secrecy::{zeroize::Zeroize, ExposeSecret, SecretString};
use std::net::SocketAddr;
use std::pin::{pin, Pin};
use std::sync::LazyLock;
use std::task::{Context, Poll};
use tracing::*;

pub struct AuthenticatedSession {
    pub cc: channel::AsyncServerControl,
    pub ec: channel::AsyncServerEvent,
    pub mc: channel::AsyncServerMedia,
    pub tc: channel::AsyncServerTunnel,
    pub client_init: cm::ClientInit,
    pub remote_addr: SocketAddr,
}

pub struct AuthenticatedStream {
    bind_addr: SocketAddr,
    inner: BoxStream<'static, Result<AuthenticatedSession>>,
}

impl Stream for AuthenticatedStream {
    type Item = Result<AuthenticatedSession>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        pin!(&mut self.inner).poll_next_unpin(cx)
    }
}

impl AuthenticatedStream {
    pub fn local_addr(&self) -> &SocketAddr {
        &self.bind_addr
    }
}

impl AuthenticatedStream {
    pub fn new(
        channel_server: channel::endpoint::AsyncServer,
        auth_token: SecretString,
    ) -> Result<Self> {
        static SALT: LazyLock<SaltString> =
            LazyLock::new(|| SaltString::generate(rand::thread_rng()));
        let auth_token_hash = Pbkdf2
            .hash_password(auth_token.expose_secret().as_ref(), &*SALT)
            .expect("hash auth token");
        let bind_addr = channel_server.local_addr()?;
        let inner = stream::try_unfold(channel_server, move |mut channel_server| {
            let auth_token_hash = auth_token_hash.clone();
            async move {
                loop {
                    info!("Listening for client");
                    let unauthed_session = channel_server.accept().await?;
                    let remote_addr = unauthed_session.remote_addr()?;
                    let client_addr = remote_addr.ip().to_string();
                    let client_port = remote_addr.port();

                    info!(%client_addr, %client_port, "Accepted client");
                    let (mut cc, ec, mc, tc) = unauthed_session.split();
                    let mut client_init = match cc.receive().await {
                        Ok(cm_c2s::Message::ClientInit(client_init)) => pin![client_init],
                        Err(error) => {
                            warn!(%client_addr, %client_port, %error, "Failed receiving ClientInit on CC");
                            continue;
                        }
                        _ => {
                            warn!(%client_addr, %client_port, "Non-ClientInit message received on CC");
                            continue;
                        }
                    };
                    trace!("Client init: {:#?}", client_init);

                    let verified = {
                        // Scoped to ensure client auth token goes out of scope and is zeroed
                        let client_at = SecretString::from(client_init.auth_token.clone());
                        client_init.auth_token.zeroize();
                        Pbkdf2
                            .verify_password(client_at.expose_secret().as_ref(), &auth_token_hash)
                            .is_ok()
                    };

                    if !verified {
                        warn!(%client_addr, %client_port, "Authentication failure");
                        server::send_flush_goodbye(&mut cc, cm::ServerGoodbyeReason::AuthenticationFailure).await;
                        continue;
                    }

                    let authed_session = AuthenticatedSession {
                        cc, ec, mc, tc, client_init: client_init.clone(), remote_addr
                    };
                    break Ok(Some((authed_session, channel_server)));
                }
            }
        }).boxed();
        Ok(AuthenticatedStream { bind_addr, inner })
    }
}
