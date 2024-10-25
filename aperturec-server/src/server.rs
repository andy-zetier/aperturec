use crate::backend::{Backend, LockState, SwapableBackend};
use crate::metrics::EncoderCount;
use crate::task::{
    backend, control_channel_handler as cc_handler, encoder, event_channel_handler as ec_handler,
    frame_sync, media_channel_handler as mc_handler, rate_limit,
    tunnel_channel_handler as tc_handler,
};

use aperturec_channel::{
    self as channel, server::states as channel_states, AsyncReceiver, AsyncSender,
    AsyncUnifiedServer,
};
use aperturec_graphics::{partition::partition, prelude::*};
use aperturec_protocol::common::*;
use aperturec_protocol::control::{
    self as cm, client_to_server as cm_c2s, server_to_client as cm_s2c,
};
use aperturec_state_machine::*;

use anyhow::{anyhow, bail, Result};
use derive_builder::Builder;
use futures::prelude::*;
use futures::stream::{FuturesUnordered, StreamExt};
use ndarray::AssignElem;
use pbkdf2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Pbkdf2,
};
use petname::{Generator, Petnames};
use secrecy::{zeroize::Zeroize, ExposeSecret, SecretString};
use std::collections::BTreeMap;
use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::pin::pin;
use std::sync::LazyLock;
use tokio::process::Command;
use tokio::sync::mpsc;
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
    #[builder(setter(strip_option), default)]
    root_process_cmdline: Option<String>,
    allow_client_exec: bool,
    mbps_max: Option<usize>,
}

#[derive(Stateful, Debug)]
#[state(S)]
pub struct Server<S: State> {
    state: S,
    config: Configuration,
    auth_token_hash: PasswordHash<'static>,
    next_client_id: u64,
}

#[derive(State, Debug)]
pub struct Created {
    root_process_cmd: Option<Command>,
    tls_material: channel::tls::Material,
}
impl SelfTransitionable for Server<Created> {}

#[derive(State, Debug)]
pub struct BackendInitialized<B: Backend> {
    backend: B,
    tls_material: channel::tls::Material,
}
impl<B: Backend> SelfTransitionable for Server<BackendInitialized<B>> {}

#[derive(State, Debug)]
pub struct Listening<B: Backend> {
    backend: B,
    channel_server: channel::Server<channel_states::AsyncListening>,
}
impl<B: Backend> SelfTransitionable for Server<Listening<B>> {}

#[derive(State, Debug)]
pub struct Accepted<B: Backend> {
    backend: B,
    channel_server: channel::Server<channel_states::AsyncAccepted>,
}

#[derive(State, Debug)]
pub struct AuthenticatedClient<B: Backend> {
    backend: SwapableBackend<B>,
    cc: channel::AsyncServerControl,
    ec: channel::AsyncServerEvent,
    mc: channel::AsyncServerMedia,
    tc: channel::AsyncServerTunnel,
    channel_server: channel::Server<channel_states::AsyncListening>,
    encoder_areas: Vec<(Box2D, Codec)>,
    resolution: Size,
    rl_config: rate_limit::Configuration,
    tunnels: BTreeMap<u64, tc_handler::Tunnel>,
}

#[derive(State)]
pub struct Running<B: Backend> {
    channel_server: channel::Server<channel_states::AsyncListening>,
    cc_tx: mpsc::Sender<cm_s2c::Message>,
    backend_task: backend::Task<backend::Running<B>>,
    cc_handler_task: cc_handler::Task<cc_handler::Running>,
    ec_handler_task: ec_handler::Task<ec_handler::Running>,
    mc_handler_task: mc_handler::Task<mc_handler::Running>,
    tc_handler_task: tc_handler::Task<tc_handler::Running>,
    rate_limit_task: rate_limit::Task<rate_limit::Running>,
    frame_sync_task: frame_sync::Task<frame_sync::Running>,
    encoder_tasks: Vec<encoder::Task<encoder::Running>>,
}

#[derive(State, Debug)]
pub struct SessionTerminated<B: Backend> {
    backend: Option<SwapableBackend<B>>,
    channel_server: channel::Server<channel_states::AsyncListening>,
}
impl<B: Backend> SelfTransitionable for Server<SessionTerminated<B>> {}

#[derive(State)]
pub struct SessionUnresumable;

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
                            warn!("existing TLS material is present but is invalid for provided external addresses");
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
            println!(
                "Generated Authentication Token: {}",
                auth_token.expose_secret()
            );
            auth_token
        });

        static SALT: LazyLock<SaltString> =
            LazyLock::new(|| SaltString::generate(rand::thread_rng()));
        let auth_token_hash = Pbkdf2
            .hash_password(auth_token.expose_secret().as_ref(), &*SALT)
            .map_err(|e| anyhow!(e))?;

        Ok(Server {
            auth_token_hash,
            state: Created {
                root_process_cmd,
                tls_material,
            },
            config,
            next_client_id: 0,
        })
    }
}

impl<B: Backend> Server<Accepted<B>> {
    pub fn remote_addr(&self) -> Result<SocketAddr> {
        Ok(self.state.channel_server.remote_addr()?)
    }
}

impl<B: Backend> AsyncTryTransitionable<BackendInitialized<B>, Created> for Server<Created> {
    type SuccessStateful = Server<BackendInitialized<B>>;
    type FailureStateful = Server<Created>;
    type Error = anyhow::Error;

    async fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let backend = try_recover_async!(
            B::initialize(
                self.config.max_width,
                self.config.max_height,
                self.state.root_process_cmd.as_mut(),
            ),
            self
        );
        Ok(Server {
            auth_token_hash: self.auth_token_hash,
            state: BackendInitialized {
                backend,
                tls_material: self.state.tls_material,
            },
            config: self.config,
            next_client_id: self.next_client_id,
        })
    }
}

impl<B: Backend> TryTransitionable<Listening<B>, BackendInitialized<B>>
    for Server<BackendInitialized<B>>
{
    type SuccessStateful = Server<Listening<B>>;
    type FailureStateful = Server<BackendInitialized<B>>;
    type Error = anyhow::Error;

    fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let pem_material: channel::tls::PemMaterial =
            try_recover!(self.state.tls_material.clone().try_into(), self);
        let channel_server = try_recover!(
            channel::server::Builder::default()
                .bind_addr(&self.config.bind_addr)
                .tls_pem_certificate(&pem_material.certificate)
                .tls_pem_private_key(&pem_material.pkey)
                .build_async(),
            self
        );
        Ok(Server {
            auth_token_hash: self.auth_token_hash,
            state: Listening {
                backend: self.state.backend,
                channel_server,
            },
            config: self.config,
            next_client_id: self.next_client_id,
        })
    }
}

impl<B: Backend> AsyncTryTransitionable<Accepted<B>, SessionTerminated<B>>
    for Server<Listening<B>>
{
    type SuccessStateful = Server<Accepted<B>>;
    type FailureStateful = Server<SessionTerminated<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let channel_server = try_transition_inner_recover_async!(
            self.state.channel_server,
            channel_states::AsyncListening,
            |channel_server| async {
                Server {
                    auth_token_hash: self.auth_token_hash,
                    state: SessionTerminated {
                        backend: Some(self.state.backend.into()),
                        channel_server,
                    },
                    config: self.config,
                    next_client_id: self.next_client_id,
                }
            }
        );
        Ok(Server {
            auth_token_hash: self.auth_token_hash,
            state: Accepted {
                backend: self.state.backend,
                channel_server,
            },
            config: self.config,
            next_client_id: self.next_client_id,
        })
    }
}

impl<B: Backend> Transitionable<SessionTerminated<B>> for Server<Accepted<B>> {
    type NextStateful = Server<SessionTerminated<B>>;

    fn transition(self) -> Self::NextStateful {
        let channel_server = transition!(self.state.channel_server, channel_states::AsyncListening);

        Server {
            auth_token_hash: self.auth_token_hash,
            state: SessionTerminated {
                backend: Some(self.state.backend.into()),
                channel_server,
            },
            config: self.config,
            next_client_id: self.next_client_id,
        }
    }
}

impl<B: Backend> AsyncTryTransitionable<AuthenticatedClient<B>, SessionTerminated<B>>
    for Server<Accepted<B>>
{
    type SuccessStateful = Server<AuthenticatedClient<B>>;
    type FailureStateful = Server<SessionTerminated<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        macro_rules! recover_self {
            ($cc:expr, $ec:expr, $mc:expr, $tc:expr, $listener:expr) => {{
                recover_self!($cc, $ec, $mc, $tc, $listener, self.state.backend)
            }};
            ($cc:expr, $ec:expr, $mc:expr, $tc:expr, $listener:expr, $backend: expr) => {{
                let server: channel::Server<_> =
                    AsyncUnifiedServer::unsplit($cc, $ec, $mc, $tc, $listener);
                Server {
                    auth_token_hash: self.auth_token_hash,
                    state: SessionTerminated {
                        backend: Some($backend.into()),
                        channel_server: transition!(server, channel_states::AsyncListening),
                    },
                    config: self.config,
                    next_client_id: self.next_client_id,
                }
            }};
        }

        let (mut cc, ec, mc, tc, listener) = try_transition_inner_recover_async!(
            self.state.channel_server,
            channel_states::AsyncReady,
            channel_states::AsyncAccepted,
            |channel_server: channel::Server<channel_states::AsyncAccepted>| future::ready(
                Server {
                    auth_token_hash: self.auth_token_hash,
                    state: SessionTerminated {
                        backend: Some(self.state.backend.into()),
                        channel_server: transition!(channel_server, channel_states::AsyncListening),
                    },
                    config: self.config,
                    next_client_id: self.next_client_id,
                }
            )
        )
        .split();

        let msg = try_recover_async!(
            cc.receive(),
            recover_self!(cc, ec, mc, tc, listener),
            SessionTerminated::<B>
        );

        let mut client_init = match msg {
            cm_c2s::Message::ClientInit(client_init) => pin![client_init],
            _ => return_recover!(
                recover_self!(cc, ec, mc, tc, listener),
                SessionTerminated::<B>,
                "non client init message received"
            ),
        };
        trace!("Client init: {:#?}", client_init);

        let verified = {
            // Scoped to ensure client auth token goes out of scope and is zeroed
            let client_at = SecretString::from(client_init.auth_token.clone());
            client_init.auth_token.zeroize();
            Pbkdf2
                .verify_password(client_at.expose_secret().as_ref(), &self.auth_token_hash)
                .is_ok()
        };

        if !verified {
            let msg = cm::ServerGoodbye::from(cm::ServerGoodbyeReason::AuthenticationFailure);
            try_recover_async!(
                cc.send(msg.into()),
                recover_self!(cc, ec, mc, tc, listener),
                SessionTerminated::<B>
            );
            return_recover!(
                recover_self!(cc, ec, mc, tc, listener),
                SessionTerminated::<B>,
                "incorrect authentication token"
            );
        }
        if client_init.max_decoder_count < 1 {
            let msg = cm::ServerGoodbye::from(cm::ServerGoodbyeReason::InvalidConfiguration);
            try_recover_async!(
                cc.send(msg.into()),
                recover_self!(cc, ec, mc, tc, listener),
                SessionTerminated::<B>
            );
            return_recover!(
                recover_self!(cc, ec, mc, tc, listener),
                SessionTerminated::<B>,
                "client sent invalid decoder max"
            );
        }
        let client_mbps_max: usize = match client_init.mbps_max.try_into().ok() {
            Some(mbps_max) => mbps_max,
            _ => return_recover!(
                recover_self!(cc, ec, mc, tc, listener),
                SessionTerminated::<B>,
                "Client provided invalid mbps max"
            ),
        };
        let client_resolution = match client_init
            .client_info
            .as_ref()
            .and_then(|ci| ci.display_size.clone())
        {
            Some(display_size) => display_size,
            _ => return_recover!(
                recover_self!(cc, ec, mc, tc, listener),
                SessionTerminated::<B>,
                "Client did not provide display size"
            ),
        };

        let (tunnel_responses, tunnels) =
            tc_handler::generate_responses(&client_init.tunnel_requests);

        let mut backend = SwapableBackend::new(self.state.backend);
        if !client_init.client_specified_program_cmdline.is_empty() {
            let cmdline = &client_init.client_specified_program_cmdline;

            if !self.config.allow_client_exec {
                let msg = cm::ServerGoodbye::from(cm::ServerGoodbyeReason::ClientExecDisallowed);
                try_recover!(
                    cc.send(msg.into()).await,
                    recover_self!(cc, ec, mc, tc, listener, backend.into_root()),
                    SessionTerminated::<B>
                );
                return_recover!(
                    recover_self!(cc, ec, mc, tc, listener, backend.into_root()),
                    SessionTerminated::<B>,
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
                            Some(&mut cmd),
                        )
                        .await
                    })
                    .await,
                async {
                    error!("Failed to launch process");
                    let msg = cm::ServerGoodbye::from(cm::ServerGoodbyeReason::ProcessLaunchFailed);
                    cc.send(msg.clone().into()).await.unwrap_or_else(|e| {
                        error!("Failed to send server goodbye {:?}: {}", msg, e)
                    });
                    recover_self!(cc, ec, mc, tc, listener, backend.into_root())
                }
                .await,
                SessionTerminated::<B>
            );
            backend.set_client_specified(client_backend);
        }

        let client_resolution = Size::new(
            client_resolution.width as usize,
            client_resolution.height as usize,
        );
        let (resolution, partitions) = partition(
            &client_resolution,
            client_init.max_decoder_count.try_into().unwrap(),
        );

        let preferred_codec = match client_init.client_caps {
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

        let encoder_areas = partitions
            .into_iter()
            .map(|rect| (rect, preferred_codec))
            .collect::<Vec<_>>();
        try_recover_async!(
            backend.set_resolution(&resolution),
            recover_self!(cc, ec, mc, tc, listener, backend.into_root()),
            SessionTerminated::<B>
        );
        trace!("Resolution set to {:?}", resolution);
        EncoderCount::update(encoder_areas.len() as f64);

        let decoder_areas: BTreeMap<u32, _> = encoder_areas
            .iter()
            .enumerate()
            .map(|(id, (b, ..))| {
                (
                    id as u32,
                    DecoderArea {
                        location: Some(Location::new(b.min.x as u64, b.min.y as u64)),
                        dimension: Some(Dimension::new(b.width() as u64, b.height() as u64)),
                    },
                )
            })
            .collect();

        let rl_config = rate_limit::Configuration::new(self.config.mbps_max, client_mbps_max);

        try_recover_async!(
            backend.set_lock_state(LockState {
                is_caps_locked: Some(client_init.is_caps_locked),
                is_num_locked: Some(client_init.is_num_locked),
                is_scroll_locked: Some(client_init.is_scroll_locked),
            }),
            recover_self!(cc, ec, mc, tc, listener, backend.into_root()),
            SessionTerminated::<B>
        );

        let server_init = try_recover!(
            cm::ServerInitBuilder::default()
                .client_id(self.next_client_id)
                .server_name(self.config.name.clone())
                .display_configuration(DisplayConfiguration::new(
                    0,
                    Some(Dimension::new(
                        resolution.width as u64,
                        resolution.height as u64
                    )),
                    decoder_areas
                ))
                .tunnel_responses(tunnel_responses)
                .build(),
            recover_self!(cc, ec, mc, tc, listener, backend.into_root()),
            SessionTerminated::<B>
        );
        trace!("Server init: {:#?}", server_init);

        try_recover_async!(
            cc.send(server_init.into()),
            recover_self!(cc, ec, mc, tc, listener, backend.into_root()),
            SessionTerminated::<B>
        );

        Ok(Server {
            auth_token_hash: self.auth_token_hash,
            state: AuthenticatedClient {
                backend,
                cc,
                ec,
                mc,
                tc,
                channel_server: listener,
                encoder_areas,
                resolution,
                rl_config,
                tunnels,
            },
            config: self.config,
            next_client_id: self.next_client_id + 1,
        })
    }
}

impl<B: Backend + 'static> AsyncTryTransitionable<Running<B>, SessionTerminated<B>>
    for Server<AuthenticatedClient<B>>
where
    for<'p> &'p mut Pixel24: AssignElem<<B::PixelMap as PixelMap>::Pixel>,
{
    type SuccessStateful = Server<Running<B>>;
    type FailureStateful = Server<SessionTerminated<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let (cc_handler_task, cc_handler_channels) = cc_handler::Task::new(self.state.cc);
        let (ec_handler_task, ec_handler_channels) = ec_handler::Task::new(self.state.ec);
        let (rate_limit_task, rl_handle) = rate_limit::Task::new(self.state.rl_config);
        let (frame_sync_task, frame_sync_channels) =
            frame_sync::Task::<frame_sync::Created<B>>::new(
                self.state.resolution,
                self.state.encoder_areas.len(),
            );
        let (mc_handler_task, mc_handler_channels) =
            mc_handler::Task::new(self.state.mc, rl_handle);

        let (encoder_tasks, encoder_command_txs): (Vec<_>, Vec<_>) = self
            .state
            .encoder_areas
            .iter()
            .zip(frame_sync_channels.frame_rxs.into_iter())
            .enumerate()
            .map(|(enc_id, ((area, codec), frame_sync_rx))| {
                encoder::Task::new(
                    enc_id,
                    *area,
                    *codec,
                    frame_sync_rx,
                    mc_handler_channels.mm_tx.clone(),
                )
            })
            .unzip();

        let tc_handler_task = tc_handler::Task::new(self.state.tc, self.state.tunnels);

        let backend_task = backend::Task::<backend::Created<B>>::new(
            self.state.backend,
            ec_handler_channels.event_rx,
            ec_handler_channels.to_send_tx.clone(),
            frame_sync_channels.damage_tx,
            frame_sync_channels.resolution_tx,
            encoder_command_txs,
        );

        let backend_task = try_transition_inner_recover_async!(
            backend_task,
            backend::Running::<B>,
            backend::Created::<B>,
            |backend_created: backend::Task<backend::Created::<B>>| async {
                Server {
                    state: SessionTerminated {
                        backend: Some(backend_created.into_backend()),
                        channel_server: self.state.channel_server,
                    },
                    config: self.config,
                    auth_token_hash: self.auth_token_hash,
                    next_client_id: self.next_client_id,
                }
            }
        );

        let cc_handler_task = transition!(cc_handler_task, cc_handler::Running);
        let ec_handler_task = transition!(ec_handler_task, ec_handler::Running);
        let mc_handler_task = transition!(mc_handler_task, mc_handler::Running);
        let tc_handler_task = transition!(tc_handler_task, tc_handler::Running);
        let rate_limit_task = transition!(rate_limit_task, rate_limit::Running);
        let frame_sync_task = transition!(frame_sync_task, frame_sync::Running);
        let encoder_tasks = encoder_tasks
            .into_iter()
            .map(|task| transition!(task, encoder::Running))
            .collect();

        Ok(Server {
            state: Running {
                channel_server: self.state.channel_server,
                cc_tx: cc_handler_channels.to_send_tx.clone(),
                backend_task,
                cc_handler_task,
                ec_handler_task,
                mc_handler_task,
                tc_handler_task,
                rate_limit_task,
                frame_sync_task,
                encoder_tasks,
            },
            config: self.config,
            auth_token_hash: self.auth_token_hash,
            next_client_id: self.next_client_id,
        })
    }
}

impl<B: Backend> Transitionable<SessionTerminated<B>> for Server<AuthenticatedClient<B>> {
    type NextStateful = Server<SessionTerminated<B>>;

    fn transition(self) -> Self::NextStateful {
        Server {
            config: self.config,
            auth_token_hash: self.auth_token_hash,
            next_client_id: self.next_client_id,
            state: SessionTerminated {
                backend: Some(self.state.backend),
                channel_server: self.state.channel_server,
            },
        }
    }
}

impl<B: Backend> AsyncTryTransitionable<SessionTerminated<B>, SessionTerminated<B>>
    for Server<Running<B>>
{
    type SuccessStateful = Server<SessionTerminated<B>>;
    type FailureStateful = Server<SessionTerminated<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let mut cts = vec![
            self.state.backend_task.cancellation_token().clone(),
            self.state.cc_handler_task.cancellation_token().clone(),
            self.state.ec_handler_task.cancellation_token().clone(),
            self.state.mc_handler_task.cancellation_token().clone(),
            self.state.tc_handler_task.cancellation_token().clone(),
            self.state.rate_limit_task.cancellation_token().clone(),
            self.state.frame_sync_task.cancellation_token().clone(),
        ];
        cts.extend(
            self.state
                .encoder_tasks
                .iter()
                .map(|task| task.cancellation_token().clone()),
        );

        macro_rules! result_task {
            ($task:expr, $success_map:expr, $failure_map:expr) => {
                $task.map_err($failure_map).map_ok($success_map).boxed()
            };
            ($task:expr, $success_map:expr) => {
                $task
                    .map_err(|recovered| recovered.error)
                    .map_ok($success_map)
                    .boxed()
            };
            ($task:expr) => {
                result_task!($task, |_| ())
            };
        }

        let mut backend_stream = result_task!(
            self.state.backend_task.try_transition(),
            |terminated| terminated.into_backend(),
            |recovered| (recovered.stateful.into_backend(), recovered.error)
        )
        .into_stream();
        let mut backend = None;

        let mut task_results = FuturesUnordered::new();
        task_results.push(result_task!(self.state.cc_handler_task.try_transition()));
        task_results.push(result_task!(self.state.ec_handler_task.try_transition()));
        task_results.push(result_task!(self.state.mc_handler_task.try_transition()));
        task_results.push(result_task!(self.state.tc_handler_task.try_transition()));
        task_results.push(result_task!(self.state.rate_limit_task.try_transition()));
        task_results.push(result_task!(self.state.frame_sync_task.try_transition()));
        for encoder_task in self.state.encoder_tasks {
            task_results.push(result_task!(encoder_task.try_transition()));
        }

        let mut cleanup_started = false;

        let ct = CancellationToken::new();
        let mut task_error = None;
        loop {
            tokio::select! {
                biased;
                _ = ct.cancelled(), if !cleanup_started => {
                    debug!("server cancelled");
                    let gb_reason = if task_error.is_some() {
                        cm::ServerGoodbyeReason::ShuttingDown
                    } else {
                        cm::ServerGoodbyeReason::InternalError
                    };
                    self.state.cc_tx
                        .send(cm::ServerGoodbye::from(gb_reason).into())
                        .await
                        .unwrap_or_else(|_| warn!("Control Channel closed before server could say goodbye"));
                    for ct in &cts {
                        ct.cancel();
                    }
                    cleanup_started = true;
                }
                Some(res) = task_results.next() => {
                    trace!("Task finished");
                    if task_error.is_none() && !cleanup_started {
                        if let Err(error) = res {
                            task_error = Some(anyhow!("task error: {}", error));
                        }
                    }
                    ct.cancel()
                }
                Some(res) = backend_stream.next(), if backend.is_none() && task_error.is_none() => {
                    match res {
                        Ok(be) => backend = Some(be),
                        Err((be_opt, error)) => {
                            backend = be_opt;
                            if task_error.is_none() && !cleanup_started {
                                task_error = Some(anyhow!("backend task error: {}", error));
                            }
                        }
                    }
                    ct.cancel();
                }
                else => break,
            }
        }

        debug!("Server tasks finished");

        let stateful = Server {
            config: self.config,
            auth_token_hash: self.auth_token_hash,
            next_client_id: self.next_client_id,
            state: SessionTerminated {
                backend,
                channel_server: self.state.channel_server,
            },
        };

        match task_error {
            None => Ok(stateful),
            Some(error) => Err(Recovered {
                stateful,
                error: anyhow!("server task error: {}", error),
            }),
        }
    }
}

impl<B: Backend> Transitionable<SessionTerminated<B>> for Server<Running<B>> {
    type NextStateful = Server<SessionTerminated<B>>;

    fn transition(self) -> Self::NextStateful {
        Server {
            config: self.config,
            auth_token_hash: self.auth_token_hash,
            next_client_id: self.next_client_id,
            state: SessionTerminated {
                backend: None,
                channel_server: self.state.channel_server,
            },
        }
    }
}

impl<B: Backend> TryTransitionable<Listening<B>, SessionUnresumable>
    for Server<SessionTerminated<B>>
{
    type SuccessStateful = Server<Listening<B>>;
    type FailureStateful = Server<SessionUnresumable>;
    type Error = anyhow::Error;

    fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        match self.state.backend {
            Some(backend) => Ok(Server {
                config: self.config,
                auth_token_hash: self.auth_token_hash,
                next_client_id: self.next_client_id,
                state: Listening {
                    backend: backend.into_root(),
                    channel_server: self.state.channel_server,
                },
            }),
            None => Err(Recovered {
                stateful: Server {
                    config: self.config,
                    auth_token_hash: self.auth_token_hash,
                    next_client_id: self.next_client_id,
                    state: SessionUnresumable,
                },
                error: anyhow!("backend unrecoverable"),
            }),
        }
    }
}

impl<B: Backend> Transitionable<SessionUnresumable> for Server<SessionTerminated<B>> {
    type NextStateful = Server<SessionUnresumable>;

    fn transition(self) -> Self::NextStateful {
        Server {
            config: self.config,
            auth_token_hash: self.auth_token_hash,
            next_client_id: self.next_client_id,
            state: SessionUnresumable,
        }
    }
}

impl<B: Backend> Transitionable<SessionTerminated<B>> for Server<Listening<B>> {
    type NextStateful = Server<SessionTerminated<B>>;

    fn transition(self) -> Self::NextStateful {
        Server {
            config: self.config,
            auth_token_hash: self.auth_token_hash,
            next_client_id: self.next_client_id,
            state: SessionTerminated {
                backend: Some(self.state.backend.into()),
                channel_server: self.state.channel_server,
            },
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::backend::X;

    use aperturec_channel::{
        tls, AsyncClientControl, AsyncClientEvent, AsyncClientMedia, AsyncClientTunnel,
        AsyncUnifiedClient,
    };
    use aperturec_protocol::control::*;

    use rand::{distributions::Alphanumeric, Rng};
    use test_log::test;

    fn client_init_msg(auth_token: SecretString) -> cm_c2s::Message {
        ClientInitBuilder::default()
            .auth_token(auth_token.expose_secret())
            .client_info(
                ClientInfoBuilder::default()
                    .version(SemVer::new(0, 1, 2))
                    .build_id("asdf")
                    .os(Os::Linux)
                    .os_version("Bionic Beaver")
                    .ssl_library("OpenSSL")
                    .ssl_version("1.2")
                    .bitness(Bitness::B64)
                    .endianness(Endianness::Big)
                    .architecture(Architecture::X86)
                    .cpu_id("Haswell")
                    .number_of_cores(4_u32)
                    .amount_of_ram("2.4Gb")
                    .display_size(
                        DimensionBuilder::default()
                            .width(1024_u64)
                            .height(768_u64)
                            .build()
                            .expect("Dimension build"),
                    )
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

    async fn raw_client(
        server_port: u16,
        cert: &str,
    ) -> channel::Client<channel::client::states::AsyncReady> {
        let client = channel::client::Builder::default()
            .server_addr("127.0.0.1")
            .server_port(server_port)
            .additional_tls_pem_certificate(cert)
            .build_async()
            .expect("client async build");
        let client = try_transition_async!(client, channel::client::states::AsyncConnected)
            .map_err(|r| r.error)
            .expect("connect");
        let client =
            try_transition_async!(client, channel::client::states::AsyncReady).expect("ready");
        client
    }

    async fn server_client_connected() -> (
        Server<Accepted<X>>,
        AsyncClientControl,
        AsyncClientEvent,
        AsyncClientMedia,
        AsyncClientTunnel,
        SecretString,
    ) {
        let tlsdir = tempdir::TempDir::new("").expect("temp dir");
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
            .allow_client_exec(false)
            .mbps_max(500.into())
            .build()
            .expect("Configuration build");
        let server: Server<Listening<X>> = Server::new(server_config.clone())
            .expect("Create server instance")
            .try_transition()
            .await
            .expect("Failed initialize X backend")
            .try_transition()
            .expect("Failed to listen");

        let mut cert_path = tlsdir.path().to_path_buf();
        cert_path.push("cert.pem");
        let mut pkey_path = tlsdir.path().to_path_buf();
        pkey_path.push("pkey.pem");
        let tls_material: tls::PemMaterial = tls::Material::from_pem_files(&cert_path, &pkey_path)
            .expect("load tls materials")
            .try_into()
            .expect("PEM");

        let client = raw_client(
            server
                .state
                .channel_server
                .local_addr()
                .expect("local addr")
                .port(),
            &tls_material.certificate,
        )
        .await;
        let server = try_transition_async!(server, Accepted<X>).expect("accept");
        let (cc, ec, mc, tc, _) = client.split();
        (server, cc, ec, mc, tc, auth_token)
    }

    async fn server_client_authenticated() -> (
        Server<AuthenticatedClient<X>>,
        AsyncClientControl,
        AsyncClientEvent,
        AsyncClientMedia,
        AsyncClientTunnel,
    ) {
        let (server, mut client_cc, client_ec, client_mc, client_tc, auth_token) =
            server_client_connected().await;
        client_cc
            .send(client_init_msg(auth_token))
            .await
            .expect("send ClientInit");
        let server = server.try_transition().await.expect("failed to auth");
        (server, client_cc, client_ec, client_mc, client_tc)
    }

    #[test(tokio::test(flavor = "multi_thread"))]
    async fn auth_fail() {
        let (server, mut client_cc, ..) = server_client_connected().await;
        client_cc
            .send(client_init_msg(SecretString::default()))
            .await
            .expect("send ClientInit");
        try_transition_async!(server, AuthenticatedClient<X>).expect_err("client authenticate");
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

        assert_eq!(dc.areas.keys().len(), 2);
        assert_eq!(
            dc.areas
                .get(&0)
                .expect("area 0")
                .dimension
                .as_ref()
                .expect("dimension")
                .width,
            512
        );
        assert_eq!(
            dc.areas
                .get(&1)
                .expect("area 1")
                .dimension
                .as_ref()
                .expect("dimension")
                .width,
            512
        );
        assert_eq!(
            dc.areas
                .get(&0)
                .expect("area 0")
                .dimension
                .as_ref()
                .expect("dimension")
                .height,
            768
        );
        assert_eq!(
            dc.areas
                .get(&1)
                .expect("area 1")
                .dimension
                .as_ref()
                .expect("dimension")
                .height,
            768
        );
        assert_eq!(
            dc.areas
                .get(&0)
                .expect("area 0")
                .location
                .as_ref()
                .expect("location")
                .x_position,
            0
        );
        assert_eq!(
            dc.areas
                .get(&1)
                .expect("area 1")
                .location
                .as_ref()
                .expect("location")
                .x_position,
            512
        );
        assert_eq!(
            dc.areas
                .get(&0)
                .expect("area 0")
                .location
                .as_ref()
                .expect("location")
                .y_position,
            0
        );
        assert_eq!(
            dc.areas
                .get(&1)
                .expect("area 1")
                .location
                .as_ref()
                .expect("location")
                .y_position,
            0
        );
        assert_eq!(dc.display_size.as_ref().expect("display_size").width, 1024);
        assert_eq!(dc.display_size.as_ref().expect("display_size").height, 768);
    }
}
