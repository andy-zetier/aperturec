use crate::backend::{Backend, LockState, SwapableBackend};
use crate::metrics::{EncoderCount, clear_session_metrics};
use crate::process_utils::DisplayableExitStatus;
use crate::session;
use crate::task::{
    backend, control_channel_handler as cc_handler, encoder, event_channel_handler as ec_handler,
    frame_sync, malloc_trim, media_channel_handler as mc_handler, rate_limit,
    tunnel_channel_handler as tc_handler,
};

use aperturec_channel::{self as channel, AsyncFlushable, AsyncSender};
use aperturec_graphics::{display::*, partition::partition_displays, prelude::*};
use aperturec_protocol::common::*;
use aperturec_protocol::control as cm;
use aperturec_state_machine::*;

use anyhow::{Result, anyhow, bail};
use derive_builder::Builder;
use derive_more::Debug;
use futures::prelude::*;
use futures::stream::{FuturesUnordered, Peekable, StreamExt};
use petname::{Generator, Petnames};
use secrecy::{ExposeSecret, SecretString};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::PathBuf;
use std::pin::{Pin, pin};
use std::sync::LazyLock;
use std::time::Duration;
use tokio::process::Command;
use tokio::time;
use tokio_util::sync::CancellationToken;
use tracing::*;

#[derive(Debug, Clone)]
pub enum TlsConfiguration {
    Provided {
        certificate_path: PathBuf,
        private_key_path: PathBuf,
    },
    Generated {
        save_directory: PathBuf,
        external_addresses: Vec<String>,
    },
}

#[derive(Builder, Debug, Clone)]
pub struct Configuration {
    name: String,
    bind_addr: String,
    tls_configuration: TlsConfiguration,
    #[builder(setter(strip_option), default)]
    auth_token: Option<SecretString>,
    max_width: usize,
    max_height: usize,
    max_display_count: usize,
    #[builder(setter(strip_option), default)]
    root_process_cmdline: Option<String>,
    allow_client_exec: bool,
    mbps_max: Option<usize>,
    #[builder(setter(strip_option), default)]
    inactivity_timeout: Option<Duration>,
}

#[derive(Stateful, Debug)]
#[state(S)]
pub struct Server<S: State> {
    state: S,
    config: Configuration,
    next_client_id: u64,
    cancellation_token: CancellationToken,
}

#[derive(State, Debug)]
pub struct Created {
    auth_token: SecretString,
    root_process_cmd: Option<Command>,
    tls_material: channel::tls::Material,
}
impl SelfTransitionable for Server<Created> {}

#[derive(State, Debug)]
pub struct BackendInitialized<B: Backend> {
    auth_token: SecretString,
    backend: B,
    tls_material: channel::tls::Material,
}
impl<B: Backend> SelfTransitionable for Server<BackendInitialized<B>> {}

#[derive(State, Debug)]
pub struct SessionInactive<B: Backend> {
    backend: B,
    #[debug(skip)]
    session_stream: Peekable<session::AuthenticatedStream>,
}

#[derive(State, Debug)]
pub struct SessionActive<B: Backend> {
    backend: SwapableBackend<B>,
    #[debug(skip)]
    session: session::AuthenticatedSession,
    #[debug(skip)]
    session_stream: Peekable<session::AuthenticatedStream>,
    displays: Vec<Display>,
    max_encoder_count: usize,
    encoder_areas: Vec<Vec<Box2D>>,
    codec: Codec,
    rl_config: rate_limit::Configuration,
    tunnels: BTreeMap<u64, tc_handler::Tunnel>,
}

#[derive(State, Debug)]
pub struct SessionTerminated<B: Backend> {
    backend: Option<SwapableBackend<B>>,
    #[debug(skip)]
    session_stream: Peekable<session::AuthenticatedStream>,
}

#[derive(State, Debug)]
pub struct SessionUnresumable;

#[derive(Debug, PartialEq)]
pub struct ServerStopped;

impl fmt::Display for ServerStopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        write!(f, "server stopped")
    }
}

impl std::error::Error for ServerStopped {}

#[derive(Clone)]
pub struct Handle(CancellationToken);

impl Handle {
    pub fn stop(&self) {
        self.0.cancel();
    }

    pub fn is_stopped(&self) -> bool {
        self.0.is_cancelled()
    }
}

fn parse_create_command(cmdline: &str) -> Result<Command> {
    if let Some(tokens) = shlex::split(cmdline) {
        let mut envs = vec![];
        let mut args = vec![];
        let mut prog = None;

        for token in tokens {
            if prog.is_none() && token.contains('=') {
                envs.push(
                    token
                        .split_once('=')
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .unwrap(),
                );
            } else if prog.is_none() {
                prog = Some(token);
            } else {
                args.push(token);
            }
        }

        let prog = prog.ok_or(anyhow!(
            "No program specified in root program \"{}\"",
            cmdline
        ))?;

        if let Err(e) = which::which(&prog) {
            anyhow::bail!(
                "Cannot launch root program \"{}\" from command-line \"{}\": {}",
                prog,
                cmdline,
                e
            );
        }

        let mut cmd = Command::new(prog);
        cmd.args(args);
        cmd.envs(envs);
        cmd.kill_on_drop(true);
        Ok(cmd)
    } else {
        Err(anyhow!("Could not parse root program \"{}\"", cmdline))
    }
}

fn get_tls_material(config: &TlsConfiguration) -> Result<channel::tls::Material> {
    match config {
        TlsConfiguration::Provided {
            certificate_path,
            private_key_path,
        } => Ok(channel::tls::Material::from_pem_files(
            certificate_path,
            private_key_path,
        )?),
        TlsConfiguration::Generated {
            save_directory,
            external_addresses,
        } => {
            const CERT_PEM_FILENAME: &str = "cert.pem";
            const KEY_PEM_FILENAME: &str = "pkey.pem";

            let mut domain_names = vec![];
            let mut ip_addresses = vec![];
            for addr in external_addresses {
                if addr.parse::<IpAddr>().is_err() {
                    domain_names.push(addr);
                } else {
                    ip_addresses.push(addr);
                }
            }

            if save_directory.exists() && !save_directory.is_dir() {
                bail!("{} is not a directory", save_directory.display());
            }

            if !save_directory.exists() {
                fs::create_dir_all(save_directory)?;
            }

            let mut cert_pem_path = save_directory.to_path_buf();
            cert_pem_path.push(CERT_PEM_FILENAME);

            let mut key_pem_path = save_directory.to_path_buf();
            key_pem_path.push(KEY_PEM_FILENAME);

            if cert_pem_path.is_file() && key_pem_path.is_file() {
                match channel::tls::Material::from_pem_files(&cert_pem_path, &key_pem_path) {
                    Ok(tls_material) => {
                        let sans = domain_names.iter().chain(ip_addresses.iter());
                        if tls_material.is_valid_for_sans(sans) {
                            info!("existing TLS material is valid, not regenerating");
                            return Ok(tls_material);
                        } else {
                            warn!(
                                "existing TLS material is present but is invalid for provided external addresses"
                            );
                        }
                    }
                    Err(e) => {
                        warn!("error loading existing TLS material: '{}'", e);
                    }
                }
            }

            info!("Generating TLS material");
            let tls_material = channel::tls::Material::ec_self_signed(domain_names, ip_addresses)?;
            let pem_material: channel::tls::PemMaterial = tls_material.clone().try_into()?;

            info!(
                "Writing self-signed certificate to {}",
                cert_pem_path.display()
            );
            fs::write(&cert_pem_path, &pem_material.certificate)?;

            info!("Writing private key to {}", key_pem_path.display());
            fs::write(&key_pem_path, &pem_material.pkey)?;

            Ok(tls_material)
        }
    }
}

impl<S: State> Server<S> {
    pub fn handle(&self) -> Handle {
        Handle(self.cancellation_token.clone())
    }
}

impl Server<Created> {
    pub fn new(mut config: Configuration) -> Result<Self> {
        let root_process_cmd = if let Some(ref rp) = config.root_process_cmdline {
            Some(parse_create_command(rp)?)
        } else {
            None
        };

        let tls_material = get_tls_material(&config.tls_configuration)?;

        static DICTIONARY: LazyLock<Petnames> = LazyLock::new(Petnames::medium);
        const NUM_WORDS: u8 = 3;
        let auth_token = config.auth_token.take().unwrap_or_else(|| {
            debug!(cardinality = DICTIONARY.cardinality(NUM_WORDS));
            let auth_token = SecretString::new(
                DICTIONARY
                    .generate(&mut rand::thread_rng(), NUM_WORDS, "-")
                    .expect("generate auth token")
                    .into(),
            );
            info!("Generated Authentication Token available via stdout");
            print!("{}", auth_token.expose_secret());
            io::stdout().flush().expect("flush stdout");

            // SAFETY: Close itself is not memory unsafe. There may be some strange behavior
            // if we try to use stdout again (e.g. writes do not actually work), but that is
            // outside the scope of the safety of this call.
            #[cfg(unix)]
            unsafe {
                libc::close(libc::STDOUT_FILENO);
            }
            auth_token
        });

        Ok(Server {
            state: Created {
                auth_token,
                root_process_cmd,
                tls_material,
            },
            cancellation_token: CancellationToken::new(),
            config,
            next_client_id: 0,
        })
    }
}

impl<B: Backend> AsyncTryTransitionable<BackendInitialized<B>, Created> for Server<Created> {
    type SuccessStateful = Server<BackendInitialized<B>>;
    type FailureStateful = Server<Created>;
    type Error = anyhow::Error;

    async fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let backend_fut = B::initialize(
            self.config.max_width,
            self.config.max_height,
            self.config.max_display_count,
            self.state.root_process_cmd.as_mut(),
        );
        tokio::select! {
            _ = self.cancellation_token.cancelled() => Err(Recovered {
                stateful: self,
                error: ServerStopped.into()
            }),
            backend_res = backend_fut => {
                match backend_res {
                    Ok(backend) => Ok(Server {
                        cancellation_token: self.cancellation_token,
                        config: self.config,
                        next_client_id: self.next_client_id,
                        state: BackendInitialized {
                            auth_token: self.state.auth_token,
                            backend,
                            tls_material: self.state.tls_material,
                        },
                    }),
                    Err(error) => Err(Recovered {
                        stateful: self,
                        error,
                    })
                }
            }
        }
    }
}

impl<B: Backend> AsyncTryTransitionable<SessionInactive<B>, BackendInitialized<B>>
    for Server<BackendInitialized<B>>
{
    type SuccessStateful = Server<SessionInactive<B>>;
    type FailureStateful = Server<BackendInitialized<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        if self.cancellation_token.is_cancelled() {
            Err(Recovered {
                stateful: self,
                error: ServerStopped.into(),
            })
        } else {
            let pem_material: channel::tls::PemMaterial =
                try_recover!(self.state.tls_material.clone().try_into(), self);
            let channel_server = try_recover!(
                channel::endpoint::ServerBuilder::default()
                    .bind_addr(&self.config.bind_addr)
                    .tls_pem_certificate(&pem_material.certificate)
                    .tls_pem_private_key(&pem_material.pkey)
                    .build_async(),
                self
            );
            let session_stream = try_recover!(
                session::AuthenticatedStream::new(channel_server, self.state.auth_token.clone()),
                self
            )
            .peekable();

            if let Err(e) = self.state.backend.clear_focus().await {
                warn!(%e, "failed to clear X focus");
            }

            Ok(Server {
                state: SessionInactive {
                    backend: self.state.backend,
                    session_stream,
                },
                config: self.config,
                next_client_id: self.next_client_id,
                cancellation_token: self.cancellation_token,
            })
        }
    }
}

pub(crate) const CHANNEL_FLUSH_TIMEOUT: Duration = Duration::from_millis(500);

pub(crate) async fn send_flush_goodbye(
    cc: &mut channel::AsyncServerControl,
    reason: cm::ServerGoodbyeReason,
) {
    time::timeout(CHANNEL_FLUSH_TIMEOUT, async {
        cc.send(cm::ServerGoodbye::from(reason).into())
            .await
            .unwrap_or_else(|error| warn!(%error, "send ServerGoodbye"));
        cc.flush()
            .await
            .unwrap_or_else(|error| debug!(%error, "flush control channel"));
    })
    .await
    .unwrap_or_else(|_| {
        warn!("Timeout sending and flushing Control Channel. Client may already be dead")
    });
}

impl<B: Backend> AsyncTryTransitionable<SessionActive<B>, SessionTerminated<B>>
    for Server<SessionInactive<B>>
{
    type SuccessStateful = Server<SessionActive<B>>;
    type FailureStateful = Server<SessionTerminated<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        macro_rules! recover_self_with_backend {
            ($backend: expr) => {{
                Server {
                    state: SessionInactive {
                        backend: $backend,
                        session_stream: self.state.session_stream,
                    },
                    config: self.config,
                    next_client_id: self.next_client_id,
                    cancellation_token: self.cancellation_token,
                }
            }};
        }

        let mut backend = pin!(&mut self.state.backend);
        let mut session = tokio::select! {
            session_res = &mut self.state.session_stream.try_next() => {
                match session_res {
                    Ok(Some(session)) => session,
                    Ok(None) => return_recover!(self, "Authenticated session stream empty"),
                    Err(e) => return_recover!(self, "Failed accepting new sessions: {}", e),
                }
            }
            root_process_es_res = backend.wait_root_process() => {
                match root_process_es_res {
                    // Clean exit – treat as a request for graceful shutdown
                    Ok(es) if es.success() => {
                        debug!("Root process exited cleanly (status {}) – shutting server down",
                               es.display());
                        return Err(Recovered {
                            stateful: transition!(self, SessionTerminated::<_>),
                            error: ServerStopped.into(),
                        });
                    }
                    // Abnormal exit
                    Ok(es) => {
                        return_recover!(self,
                            "Root process exited with status: {}", es.display());
                    }
                    // Waiting on the process itself failed
                    Err(e) => {
                        return_recover!(self,
                            "Failed waiting on root process: {}", e);
                    }
                }
            }
            _ = self.cancellation_token.cancelled() => {
                return Err(Recovered {
                    stateful: transition!(self, SessionTerminated::<_>),
                    error: ServerStopped.into(),
                });
            }
        };
        info!(
            client_address = %session.remote_addr.ip().to_string(),
            client_port = %session.remote_addr.port(),
            "Authenticated client"
        );
        if session.client_init.max_decoder_count < 1 {
            send_flush_goodbye(
                &mut session.cc,
                cm::ServerGoodbyeReason::InvalidConfiguration,
            )
            .await;
            return_recover!(self, "client sent invalid decoder max");
        }
        let client_mbps_max: usize = match session.client_init.mbps_max.try_into().ok() {
            Some(mbps_max) => mbps_max,
            _ => {
                send_flush_goodbye(
                    &mut session.cc,
                    cm::ServerGoodbyeReason::InvalidConfiguration,
                )
                .await;
                return_recover!(self, "Client provided invalid mbps max");
            }
        };

        let Some(client_info) = session.client_init.client_info.as_ref() else {
            send_flush_goodbye(
                &mut session.cc,
                cm::ServerGoodbyeReason::InvalidConfiguration,
            )
            .await;
            return_recover!(self, "Client did not provide client info");
        };

        let Ok(client_displays) = client_info
            .as_ref()
            .iter()
            .map(|di| Display::try_from(*di))
            .collect::<Result<Vec<_>, _>>()
        else {
            send_flush_goodbye(
                &mut session.cc,
                cm::ServerGoodbyeReason::InvalidConfiguration,
            )
            .await;
            return_recover!(self, "Client provided invalid displays");
        };

        let (tunnel_responses, tunnels) =
            tc_handler::generate_responses(&session.client_init.tunnel_requests);

        let mut backend = SwapableBackend::new(self.state.backend);
        if !session
            .client_init
            .client_specified_program_cmdline
            .is_empty()
        {
            let cmdline = &session.client_init.client_specified_program_cmdline;

            if !self.config.allow_client_exec {
                send_flush_goodbye(
                    &mut session.cc,
                    cm::ServerGoodbyeReason::ClientExecDisallowed,
                )
                .await;
                return_recover!(
                    recover_self_with_backend!(backend.into_root()),
                    "Client attempted to exec '{}', but client exec is disallowed",
                    cmdline
                );
            }

            let client_backend = try_recover!(
                future::ready(parse_create_command(cmdline))
                    .and_then(|mut cmd| async move {
                        B::initialize(
                            self.config.max_width,
                            self.config.max_height,
                            self.config.max_display_count,
                            Some(&mut cmd),
                        )
                        .await
                    })
                    .await,
                async {
                    error!("Failed to launch process");
                    send_flush_goodbye(
                        &mut session.cc,
                        cm::ServerGoodbyeReason::ProcessLaunchFailed,
                    )
                    .await;
                    recover_self_with_backend!(backend.into_root())
                }
                .await
            );
            backend.set_client_specified(client_backend);
        }

        let codec = match session.client_init.client_caps {
            Some(ref caps) => {
                if caps.supported_codecs.contains(&Codec::Jpegxl.into()) {
                    Codec::Jpegxl
                } else if caps.supported_codecs.contains(&Codec::Zlib.into()) {
                    Codec::Zlib
                } else {
                    Codec::Raw
                }
            }
            None => Codec::Raw,
        };

        let max_encoder_count = session.client_init.max_decoder_count.try_into().unwrap();
        debug!(?client_displays);

        let (displays, encoder_areas) = match backend.set_displays(client_displays).await {
            Ok(success) => match partition_displays(
                &success.displays,
                session.client_init.max_decoder_count.try_into().unwrap(),
            ) {
                Ok(areas) => (success.displays, areas),
                Err(_) => {
                    send_flush_goodbye(&mut session.cc, cm::ServerGoodbyeReason::InternalError)
                        .await;
                    return_recover!(
                        recover_self_with_backend!(backend.into_root()),
                        "Failed to re-partition"
                    );
                }
            },
            Err(_) => {
                send_flush_goodbye(&mut session.cc, cm::ServerGoodbyeReason::InternalError).await;
                return_recover!(
                    recover_self_with_backend!(backend.into_root()),
                    "Failed to set_displays"
                );
            }
        };

        let Ok(display_configuration) =
            backend::display_config_from_parts(0, &displays, &encoder_areas)
        else {
            send_flush_goodbye(&mut session.cc, cm::ServerGoodbyeReason::InternalError).await;
            return_recover!(
                recover_self_with_backend!(backend.into_root()),
                "Failed to build display configuration"
            );
        };

        debug!(?display_configuration);

        let rl_config = rate_limit::Configuration::new(self.config.mbps_max, client_mbps_max);

        try_recover_async!(
            backend.set_lock_state(LockState {
                is_caps_locked: Some(session.client_init.is_caps_locked),
                is_num_locked: Some(session.client_init.is_num_locked),
                is_scroll_locked: Some(session.client_init.is_scroll_locked),
            }),
            recover_self_with_backend!(backend.into_root())
        );

        if self.cancellation_token.is_cancelled() {
            debug!("server stopped before session started, sending ServerGoodbye");
            send_flush_goodbye(&mut session.cc, cm::ServerGoodbyeReason::ShuttingDown).await;
            Err(Recovered {
                stateful: transition!(
                    recover_self_with_backend!(backend.into_root()),
                    SessionTerminated<_>
                ),
                error: ServerStopped.into(),
            })
        } else {
            let server_init = try_recover!(
                cm::ServerInitBuilder::default()
                    .client_id(self.next_client_id)
                    .server_name(self.config.name.clone())
                    .display_configuration(display_configuration)
                    .tunnel_responses(tunnel_responses)
                    .build(),
                recover_self_with_backend!(backend.into_root())
            );

            trace!("Server init: {:#?}", server_init);

            try_recover_async!(
                session.cc.send(server_init.into()),
                recover_self_with_backend!(backend.into_root())
            );
            try_recover_async!(
                session.cc.flush(),
                recover_self_with_backend!(backend.into_root())
            );

            Ok(Server {
                state: SessionActive {
                    backend,
                    session,
                    session_stream: self.state.session_stream,
                    displays,
                    max_encoder_count,
                    encoder_areas,
                    codec,
                    rl_config,
                    tunnels,
                },
                config: self.config,
                next_client_id: self.next_client_id + 1,
                cancellation_token: self.cancellation_token,
            })
        }
    }
}

impl<B: Backend + 'static> AsyncTransitionable<SessionTerminated<B>> for Server<SessionActive<B>>
where
    <B as Backend>::PixelMap: PixelMap<Pixel = Pixel32>,
{
    type NextStateful = Server<SessionTerminated<B>>;

    async fn transition(mut self) -> Self::NextStateful {
        let display_extent = self
            .state
            .displays
            .derive_extent()
            .expect("Invalid displays");
        let encoder_counts: Vec<_> = self.state.encoder_areas.iter().map(|a| a.len()).collect();
        EncoderCount::update(encoder_counts.iter().sum::<usize>() as f64);

        let (cc_handler_task, cc_handler_channels) = cc_handler::Task::new(self.state.session.cc);
        let (ec_handler_task, mut ec_handler_channels) =
            ec_handler::Task::new(self.state.session.ec, self.config.inactivity_timeout);
        let (rate_limit_task, rl_handle) = rate_limit::Task::new(self.state.rl_config);
        let (frame_sync_task, frame_sync_channels) =
            frame_sync::Task::<frame_sync::Created<B>>::new(
                self.state.max_encoder_count,
                self.state
                    .displays
                    .into_iter()
                    .zip(encoder_counts)
                    .collect(),
                frame_sync::OutputPixelFormat::Bit24,
            );
        let (mc_handler_task, mc_handler_channels) =
            mc_handler::Task::new(self.state.session.mc, rl_handle);

        let encoder_task_res = self
            .state
            .encoder_areas
            .iter()
            .flat_map(|areas| areas.iter().enumerate())
            .zip(frame_sync_channels.frame_rxs.into_iter())
            .map(|((enc_id, area), frame_sync_rx)| {
                encoder::Task::new(
                    enc_id,
                    *area,
                    self.state.codec,
                    frame_sync_rx,
                    mc_handler_channels.mm_tx.clone(),
                )
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Vec::into_iter)
            .map(Iterator::unzip);
        let (encoder_tasks, encoder_command_txs): (Vec<_>, Vec<_>) = match encoder_task_res {
            Ok((tasks, txs)) => (tasks, txs),
            Err(error) => {
                error!(%error, "failed starting encoder tasks");
                return Server {
                    state: SessionTerminated {
                        backend: Some(self.state.backend),
                        session_stream: self.state.session_stream,
                    },
                    config: self.config,
                    next_client_id: self.next_client_id,
                    cancellation_token: self.cancellation_token,
                };
            }
        };

        let tc_handler_task = tc_handler::Task::new(self.state.session.tc, self.state.tunnels);

        let backend_task = backend::Task::<backend::Created<B>>::new(
            self.state.backend,
            display_extent,
            ec_handler_channels.event_rx,
            ec_handler_channels.to_send_tx.clone(),
            frame_sync_channels.damage_tx,
            frame_sync_channels.display_config_tx,
            encoder_command_txs,
        );

        let backend_task = match backend_task.try_transition().await {
            Ok(backend_task) => backend_task,
            Err(Recovered { stateful, error }) => {
                error!(%error, "failed starting backend task");
                return Server {
                    state: SessionTerminated {
                        backend: Some(stateful.into_backend()),
                        session_stream: self.state.session_stream,
                    },
                    config: self.config,
                    next_client_id: self.next_client_id,
                    cancellation_token: self.cancellation_token,
                };
            }
        };

        let malloc_trim_task = malloc_trim::Task::default();

        let cc_handler_task = transition!(cc_handler_task, cc_handler::Running);
        let ec_handler_task = transition!(ec_handler_task, ec_handler::Running);
        let mc_handler_task = transition!(mc_handler_task, mc_handler::Running);
        let tc_handler_task = transition!(tc_handler_task, tc_handler::Running);
        let rate_limit_task = transition!(rate_limit_task, rate_limit::Running);
        let frame_sync_task = transition!(frame_sync_task, frame_sync::Running);
        let malloc_trim_task = transition!(malloc_trim_task, malloc_trim::Running);
        let encoder_tasks = encoder_tasks
            .into_iter()
            .map(|task| transition!(task, encoder::Running))
            .collect::<Vec<_>>();

        let mut cts = vec![
            backend_task.cancellation_token().clone(),
            cc_handler_task.cancellation_token().clone(),
            ec_handler_task.cancellation_token().clone(),
            mc_handler_task.cancellation_token().clone(),
            tc_handler_task.cancellation_token().clone(),
            rate_limit_task.cancellation_token().clone(),
            frame_sync_task.cancellation_token().clone(),
            malloc_trim_task.cancellation_token().clone(),
        ];
        cts.extend(
            encoder_tasks
                .iter()
                .map(|task| task.cancellation_token().clone()),
        );

        macro_rules! result_task {
            ($task:expr, $success_map:expr, $failure_map:expr) => {
                $task.map_err($failure_map).map_ok($success_map).boxed()
            };
            ($task:expr, $success_map:expr) => {
                result_task!($task, $success_map, |recovered| recovered.error)
            };
            ($task:expr) => {
                result_task!($task, |_| ())
            };
        }

        let mut backend_stream =
            result_task!(backend_task.try_transition(), backend::Task::components).into_stream();
        let mut backend_result: Option<Result<SwapableBackend<B>>> = None;

        let mut task_results = FuturesUnordered::new();
        task_results.push(result_task!(cc_handler_task.try_transition()));
        task_results.push(result_task!(ec_handler_task.try_transition()));
        task_results.push(result_task!(mc_handler_task.try_transition()));
        task_results.push(result_task!(tc_handler_task.try_transition()));
        task_results.push(result_task!(rate_limit_task.try_transition()));
        task_results.push(result_task!(frame_sync_task.try_transition()));
        task_results.push(result_task!(malloc_trim_task.try_transition()));
        task_results.extend(
            encoder_tasks
                .into_iter()
                .map(|t| result_task!(t.try_transition())),
        );

        let mut cleanup_started = false;
        let mut task_error = None;
        let mut gb_reason = None;
        let ct = CancellationToken::new();
        let cc_tx = cc_handler_channels.to_send_tx.clone();

        let mut sessions_stream = Pin::new(&mut self.state.session_stream);

        loop {
            tokio::select! {
                biased;
                _ = ct.cancelled(), if !cleanup_started => {
                    debug!("session cancelled");
                    if let Some(gb_reason) = gb_reason {
                        cc_tx
                            .send(cm::ServerGoodbye::from(gb_reason).into())
                            .await
                            .unwrap_or_else(|_| warn!("Control Channel closed before server could say goodbye"));
                    }
                    cts.iter().for_each(CancellationToken::cancel);
                    cleanup_started = true;
                }
                _ = self.cancellation_token.cancelled(), if !cleanup_started => {
                    debug!("server stopped");
                    if gb_reason.is_none() {
                        gb_reason = Some(cm::ServerGoodbyeReason::ShuttingDown);
                    }
                    ct.cancel();
                }
                _ = sessions_stream.as_mut().peek(), if !cleanup_started => {
                    debug!("new session");
                    if gb_reason.is_none() {
                        gb_reason = Some(cm::ServerGoodbyeReason::OtherLogin);
                    }
                    ct.cancel();
                }
                Ok(())= &mut ec_handler_channels.client_inactive_rx, if !cleanup_started => {
                    if gb_reason.is_none() {
                        gb_reason = Some(cm::ServerGoodbyeReason::InactiveTimeout);
                    }
                    ct.cancel();
                }
                Some(res) = task_results.next() => {
                    trace!("Task finished");
                    if task_error.is_none() && !cleanup_started {
                        if let Err(error) = res {
                            task_error = Some(anyhow!("task error: {}", error));
                            if gb_reason.is_none() {
                                gb_reason = Some(cm::ServerGoodbyeReason::InternalError);
                            }
                        }
                    }
                    ct.cancel();
                }
                Some(res) = backend_stream.next(), if backend_result.is_none() => {
                    match res {
                        Ok((mut backend, error_opt)) => {
                            if let Some(error) = error_opt {
                                warn!(%error, "error in backend task");
                                if gb_reason.is_none() {
                                    if backend.wait_root_process().now_or_never().is_some() {
                                        gb_reason = Some(cm::ServerGoodbyeReason::ProcessExited);
                                    } else {
                                        gb_reason = Some(cm::ServerGoodbyeReason::InternalError);
                                    }
                                }
                            }
                            backend_result = Some(Ok(backend));
                            ct.cancel();
                        },
                        Err(error) => backend_result = Some(Err(error)),
                    }
                    ct.cancel();
                }
                else => break
            }
        }

        debug!("Terminating");
        let backend_out = match backend_result.expect("backend result still none") {
            Ok(be) => {
                if let Err(e) = be.clear_focus().await {
                    warn!(%e, "failed to clear X focus");
                }
                Some(be)
            }
            Err(error) => {
                error!(%error, "Backend unrecoverable");
                None
            }
        };

        clear_session_metrics();

        Server {
            state: SessionTerminated {
                backend: backend_out,
                session_stream: self.state.session_stream,
            },
            config: self.config,
            next_client_id: self.next_client_id,
            cancellation_token: self.cancellation_token,
        }
    }
}

impl<B: Backend> TryTransitionable<SessionInactive<B>, SessionUnresumable>
    for Server<SessionTerminated<B>>
{
    type SuccessStateful = Server<SessionInactive<B>>;
    type FailureStateful = Server<SessionUnresumable>;
    type Error = anyhow::Error;

    fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        macro_rules! session_unresumable {
            () => {
                Server {
                    config: self.config,
                    next_client_id: self.next_client_id,
                    state: SessionUnresumable,
                    cancellation_token: self.cancellation_token,
                }
            };
        }

        match (self.state.backend, self.cancellation_token.is_cancelled()) {
            (Some(mut backend), false) => {
                if let Some(exit_status_result) = backend.wait_root_process().now_or_never() {
                    match exit_status_result {
                        Ok(exit_status) => {
                            if exit_status.success() {
                                // clean exit – treat as graceful shutdown
                                debug!(
                                    "Root process exited cleanly (status {}) – shutting server down",
                                    exit_status.display()
                                );
                                Err(Recovered {
                                    stateful: session_unresumable!(),
                                    error: ServerStopped.into(),
                                })
                            } else {
                                // abnormal exit – still an error
                                Err(Recovered {
                                    stateful: session_unresumable!(),
                                    error: anyhow!(
                                        "Root process exited with status: {}",
                                        exit_status.display()
                                    ),
                                })
                            }
                        }
                        Err(e) => Err(Recovered {
                            stateful: session_unresumable!(),
                            error: anyhow!("Failed to wait on root process: {}", e),
                        }),
                    }
                } else {
                    Ok(Server {
                        config: self.config,
                        next_client_id: self.next_client_id,
                        state: SessionInactive {
                            backend: backend.into_root(),
                            session_stream: self.state.session_stream,
                        },
                        cancellation_token: self.cancellation_token,
                    })
                }
            }
            (.., is_cancelled) => {
                let error = if is_cancelled {
                    ServerStopped.into()
                } else {
                    anyhow!("backend unrecoverable")
                };

                Err(Recovered {
                    stateful: session_unresumable!(),
                    error,
                })
            }
        }
    }
}

impl<B: Backend> Transitionable<SessionTerminated<B>> for Server<SessionInactive<B>> {
    type NextStateful = Server<SessionTerminated<B>>;

    fn transition(self) -> Self::NextStateful {
        Server {
            config: self.config,
            next_client_id: self.next_client_id,
            state: SessionTerminated {
                backend: Some(self.state.backend.into()),
                session_stream: self.state.session_stream,
            },
            cancellation_token: self.cancellation_token,
        }
    }
}

impl<B: Backend> Transitionable<SessionUnresumable> for Server<SessionTerminated<B>> {
    type NextStateful = Server<SessionUnresumable>;

    fn transition(self) -> Self::NextStateful {
        Server {
            config: self.config,
            next_client_id: self.next_client_id,
            state: SessionUnresumable,
            cancellation_token: self.cancellation_token,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::backend::X;

    use aperturec_channel::{
        AsyncClientControl, AsyncClientEvent, AsyncClientMedia, AsyncClientTunnel, AsyncReceiver,
        Unified, tls,
    };
    use aperturec_protocol::control::{client_to_server as cm_c2s, server_to_client as cm_s2c, *};

    use rand::{Rng, distributions::Alphanumeric};
    use test_log::test;

    fn client_init_msg(auth_token: SecretString) -> cm_c2s::Message {
        ClientInitBuilder::default()
            .auth_token(auth_token.expose_secret())
            .client_info(
                ClientInfoBuilder::default()
                    .version(SemVer::try_from("0.1.2-alpha+asdf").unwrap())
                    .os(Os::Linux)
                    .os_version("Bionic Beaver")
                    .bitness(Bitness::B64)
                    .endianness(Endianness::Big)
                    .architecture(Architecture::X86)
                    .cpu_id("Haswell")
                    .number_of_cores(4_u32)
                    .amount_of_ram("2.4Gb")
                    .displays([DisplayInfoBuilder::default()
                        .area(
                            RectangleBuilder::default()
                                .dimension(Dimension::new(1024_u64, 768_u64))
                                .location(Location::new(0, 0))
                                .build()
                                .expect("Rectangle build"),
                        )
                        .is_enabled(true)
                        .build()
                        .expect("DisplayInfo build")])
                    .build()
                    .expect("ClientInfo build"),
            )
            .client_caps(
                ClientCapsBuilder::default()
                    .supported_codecs(vec![Codec::Zlib.into()])
                    .build()
                    .expect("ClientCaps build"),
            )
            .max_decoder_count(3_u32)
            .build()
            .expect("ClientInit build")
            .into()
    }

    fn client_endpoint(cert: &str) -> channel::endpoint::AsyncClient {
        channel::endpoint::ClientBuilder::default()
            .additional_tls_pem_certificate(cert)
            .build_async()
            .expect("client async build")
    }

    async fn server_client_inactive() -> (
        Server<SessionInactive<X>>,
        channel::endpoint::AsyncClient,
        SecretString,
    ) {
        let tlsdir = tempfile::tempdir().expect("temp dir");
        let auth_token: SecretString = SecretString::from(
            rand::thread_rng()
                .sample_iter(&Alphanumeric)
                .take(16)
                .map(char::from)
                .collect::<String>(),
        );
        let server_config = ConfigurationBuilder::default()
            .bind_addr("127.0.0.1:0".to_string())
            .name("test server".into())
            .tls_configuration(TlsConfiguration::Generated {
                save_directory: tlsdir.path().to_path_buf(),
                external_addresses: vec![],
            })
            .auth_token(auth_token.clone())
            .max_width(1920)
            .max_height(1080)
            .max_display_count(4)
            .allow_client_exec(false)
            .mbps_max(500.into())
            .inactivity_timeout(Duration::from_secs(5))
            .build()
            .expect("Configuration build");
        let server: Server<SessionInactive<X>> = Server::new(server_config.clone())
            .expect("Create server instance")
            .try_transition()
            .await
            .expect("Failed initialize X backend")
            .try_transition()
            .await
            .expect("Failed to listen");

        let mut cert_path = tlsdir.path().to_path_buf();
        cert_path.push("cert.pem");
        let mut pkey_path = tlsdir.path().to_path_buf();
        pkey_path.push("pkey.pem");
        let tls_material: tls::PemMaterial = tls::Material::from_pem_files(&cert_path, &pkey_path)
            .expect("load tls materials")
            .try_into()
            .expect("PEM");

        let client = client_endpoint(&tls_material.certificate);
        (server, client, auth_token)
    }

    async fn server_client_authenticated() -> (
        Server<SessionActive<X>>,
        AsyncClientControl,
        AsyncClientEvent,
        AsyncClientMedia,
        AsyncClientTunnel,
    ) {
        let (server, mut client_endpoint, auth_token) = server_client_inactive().await;
        let bind_addr = server.state.session_stream.get_ref().local_addr();
        let client_session = client_endpoint
            .connect(&bind_addr.ip().to_string(), bind_addr.port())
            .await
            .expect("client connect");
        let (mut client_cc, client_ec, client_mc, client_tc) = client_session.split();
        client_cc
            .send(client_init_msg(auth_token))
            .await
            .expect("send ClientInit");
        let server = try_transition_async!(server, SessionActive<_>).expect("auth failed");
        (server, client_cc, client_ec, client_mc, client_tc)
    }

    #[test(tokio::test(flavor = "multi_thread"))]
    async fn auth_fail() {
        let (mut server, mut client_endpoint, ..) = server_client_inactive().await;
        let bind_addr = server.state.session_stream.get_ref().local_addr();
        let client_session = client_endpoint
            .connect(&bind_addr.ip().to_string(), bind_addr.port())
            .await
            .expect("client connect");
        let (mut client_cc, ..) = client_session.split();
        client_cc
            .send(client_init_msg(SecretString::default()))
            .await
            .expect("send ClientInit");
        if server.state.session_stream.next().now_or_never().is_some() {
            panic!("auth passed");
        }
    }

    #[test(tokio::test(flavor = "multi_thread"))]
    async fn auth_pass() {
        let _ = server_client_authenticated().await;
    }

    #[test(tokio::test(flavor = "multi_thread"))]
    async fn client_init() {
        let (_, mut client_cc, ..) = server_client_authenticated().await;
        let msg = client_cc.receive().await.expect("receiving server init");
        let server_init = if let cm_s2c::Message::ServerInit(server_init) = msg {
            server_init
        } else {
            panic!("non server init message");
        };

        let dc = server_init
            .display_configuration
            .expect("display_configuration");

        let dc = dc
            .display_decoder_infos
            .first()
            .expect("missing DisplayDecoderInfo 0");

        info!(?dc.decoder_areas);

        assert_eq!(dc.decoder_areas.len(), 3);
        assert_eq!(
            dc.decoder_areas
                .first()
                .expect("area 0")
                .dimension
                .as_ref()
                .expect("dimension")
                .width,
            341
        );
        assert_eq!(
            dc.decoder_areas
                .get(1)
                .expect("area 1")
                .dimension
                .as_ref()
                .expect("dimension")
                .width,
            341
        );
        assert_eq!(
            dc.decoder_areas
                .get(2)
                .expect("area 2")
                .dimension
                .as_ref()
                .expect("dimension")
                .width,
            342
        );
        assert_eq!(
            dc.decoder_areas
                .first()
                .expect("area 0")
                .dimension
                .as_ref()
                .expect("dimension")
                .height,
            768
        );
        assert_eq!(
            dc.decoder_areas
                .get(1)
                .expect("area 1")
                .dimension
                .as_ref()
                .expect("dimension")
                .height,
            768
        );
        assert_eq!(
            dc.decoder_areas
                .get(2)
                .expect("area 2")
                .dimension
                .as_ref()
                .expect("dimension")
                .height,
            768
        );
        assert_eq!(
            dc.decoder_areas
                .first()
                .expect("area 0")
                .location
                .as_ref()
                .expect("location")
                .x_position,
            0
        );
        assert_eq!(
            dc.decoder_areas
                .get(1)
                .expect("area 1")
                .location
                .as_ref()
                .expect("location")
                .x_position,
            341
        );
        assert_eq!(
            dc.decoder_areas
                .get(2)
                .expect("area 2")
                .location
                .as_ref()
                .expect("location")
                .x_position,
            682
        );
        assert_eq!(
            dc.decoder_areas
                .first()
                .expect("area 0")
                .location
                .as_ref()
                .expect("location")
                .y_position,
            0
        );
        assert_eq!(
            dc.decoder_areas
                .get(1)
                .expect("area 1")
                .location
                .as_ref()
                .expect("location")
                .y_position,
            0
        );
        assert_eq!(
            dc.decoder_areas
                .get(2)
                .expect("area 2")
                .location
                .as_ref()
                .expect("location")
                .y_position,
            0
        );
        assert_eq!(
            dc.display
                .as_ref()
                .unwrap()
                .area
                .as_ref()
                .unwrap()
                .dimension
                .as_ref()
                .expect("display_size")
                .width,
            1024
        );
        assert_eq!(
            dc.display
                .as_ref()
                .unwrap()
                .area
                .as_ref()
                .unwrap()
                .dimension
                .as_ref()
                .expect("display_size")
                .height,
            768
        );
    }
}
