use crate::backend::{Backend, SwapableBackend};
use crate::task::{
    backend, control_channel_handler as cc_handler, encoder, event_channel_handler as ec_handler,
    flow_control, heartbeat, media_channel_handler as mc_handler,
};

use anyhow::{anyhow, bail, Result};
use aperturec_channel::{
    self as channel, server::states as channel_states, AsyncReceiver, AsyncSender,
    AsyncUnifiedServer,
};
use aperturec_protocol::common::*;
use aperturec_protocol::control as cm;
use aperturec_protocol::control::client_to_server as cm_c2s;
use aperturec_protocol::control::server_to_client as cm_s2c;
use aperturec_state_machine::*;
use aperturec_trace::log;
use derive_builder::Builder;
use futures::future::{self, TryFutureExt};
use std::cmp::{max, min};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

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
    temp_client_id: u64,
    max_width: usize,
    max_height: usize,
    #[builder(setter(strip_option), default)]
    root_process_cmdline: Option<String>,
    allow_client_exec: bool,
    mbps_max: Option<usize>,
    window_size: Option<usize>,
}

#[derive(Stateful, Debug)]
#[state(S)]
pub struct Server<S: State> {
    state: S,
    config: Configuration,
}

#[derive(State, Debug)]
pub struct Created {
    root_process_cmd: Option<Command>,
    tls_material: channel::tls::Material,
}
impl SelfTransitionable for Server<Created> {}

#[derive(State, Debug)]
pub struct BackendInitialized<B: Backend + 'static> {
    backend: B,
    tls_material: channel::tls::Material,
}
impl<B: Backend + 'static> SelfTransitionable for Server<BackendInitialized<B>> {}

#[derive(State, Debug)]
pub struct Listening<B: Backend + 'static> {
    backend: B,
    channel_server: channel::Server<channel_states::AsyncListening>,
}
impl<B: Backend + 'static> SelfTransitionable for Server<Listening<B>> {}

#[derive(State, Debug)]
pub struct Accepted<B: Backend + 'static> {
    backend: B,
    channel_server: channel::Server<channel_states::AsyncAccepted>,
}

#[derive(State, Debug)]
pub struct AuthenticatedClient<B: Backend + 'static> {
    backend: SwapableBackend<B>,
    cc: channel::AsyncServerControl,
    ec: channel::AsyncServerEvent,
    mc: channel::AsyncServerMedia,
    channel_server: channel::Server<channel_states::AsyncListening>,
    client_hb_interval: Duration,
    client_hb_response_interval: Duration,
    decoders: BTreeMap<u32, (Location, Dimension, Codec)>,
    fc_config: flow_control::Configuration,
}

#[derive(State)]
pub struct Running<B: Backend + 'static> {
    ct: CancellationToken,
    channel_server: channel::Server<channel_states::AsyncListening>,
    cc_tx: mpsc::Sender<cm_s2c::Message>,
    backend_task: backend::Task<backend::Running<B>>,
    heartbeat_task: heartbeat::Task<heartbeat::Running>,
    cc_handler_task: cc_handler::Task<cc_handler::Running>,
    ec_handler_task: ec_handler::Task<ec_handler::Running>,
    mc_handler_task: mc_handler::Task<mc_handler::Running>,
    flow_control_task: flow_control::Task<flow_control::Running>,
}

#[derive(State)]
pub struct SessionComplete<B: Backend + 'static> {
    backend: SwapableBackend<B>,
    channel_server: channel::Server<channel_states::AsyncListening>,
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
            let mut domain_names = vec![];
            let mut ip_addresses = vec![];
            for addr in external_addresses {
                if addr.parse::<IpAddr>().is_err() {
                    domain_names.push(addr);
                } else {
                    ip_addresses.push(addr);
                }
            }

            let tls_material = channel::tls::Material::ec_self_signed(domain_names, ip_addresses)?;
            let pem_material: channel::tls::PemMaterial = tls_material.clone().try_into()?;

            if save_directory.exists() && !save_directory.is_dir() {
                bail!("{} is not a directory", save_directory.display());
            }

            if !save_directory.exists() {
                fs::create_dir_all(save_directory)?;
            }

            let mut path = save_directory.to_path_buf();

            path.push("cert.pem");
            log::info!(
                "Writing server self-signed certificate to {}",
                path.display()
            );
            fs::write(&path, &pem_material.certificate)?;
            path.pop();

            path.push("pkey.pem");
            log::warn!("Writing server private key to {}", path.display());
            fs::write(&path, &pem_material.pkey)?;

            Ok(tls_material)
        }
    }
}

impl Server<Created> {
    pub fn new(config: Configuration) -> Result<Self> {
        let root_process_cmd = if let Some(ref rp) = config.root_process_cmdline {
            Some(parse_create_command(rp)?)
        } else {
            None
        };

        let tls_material = get_tls_material(&config.tls_configuration)?;

        Ok(Server {
            state: Created {
                root_process_cmd,
                tls_material,
            },
            config,
        })
    }
}

impl<B: Backend + 'static> Server<Running<B>> {
    pub fn stop(&self) {
        self.state.ct.cancel();
    }
}

#[derive(Copy, Clone)]
struct FactorPair {
    w: usize,
    h: usize,
}

impl FactorPair {
    fn update<T>(&mut self, width: T, height: T)
    where
        T: Into<usize>,
    {
        self.w = width.into();
        self.h = height.into();
    }
}

fn partition(
    client_resolution: &Dimension,
    max_decoder_count: usize,
) -> (encoder::Size, Vec<encoder::Rect>) {
    let n_encoders = if max_decoder_count > 1 {
        max_decoder_count / 2 * 2
    } else {
        max_decoder_count
    };

    let width = client_resolution.width as usize;
    let height = client_resolution.height as usize;

    //
    // Gather a list of factor pairs for the given n_encoders arranged from largest aspect
    // ratio to the least.
    //
    let mut factors: VecDeque<_> = (1..=n_encoders).filter(|&x| n_encoders % x == 0).collect();

    let mut factor_pairs: Vec<FactorPair> = vec![
        FactorPair { w: 0, h: 0 };
        (if factors.len() & 0x1 == 1 {
            factors.len() + 1
        } else {
            factors.len()
        }) / 2
    ];

    for fp in factor_pairs.iter_mut() {
        let f0 = factors.pop_front().expect("pop front");
        if !factors.is_empty() {
            let f1 = factors.pop_back().expect("pop back");
            if width >= height {
                fp.update(max(f0, f1), min(f0, f1));
            } else {
                fp.update(min(f0, f1), max(f0, f1));
            }
        } else {
            fp.update(f0, f0);
        }
    }

    if !factors.is_empty() {
        panic!(
            "{} factors of {} remain! {:#?}",
            factors.len(),
            n_encoders,
            factors
        );
    }

    if factor_pairs.is_empty() {
        panic!("{} {}x{} has no factor pairs!", n_encoders, width, height);
    }

    //
    // Define a series of tests for the FactorPairs, in descending order, of how well a
    // FactorPair divides the requested width and height.
    //
    type FactorTest<'a> = dyn Fn(&FactorPair) -> bool + 'a;
    let tests: Vec<Box<FactorTest>> = vec![
        Box::new(|fp| width % fp.w == 0 && height % fp.h == 0),
        Box::new(|fp| width % fp.w == 1 && height % fp.h == 0),
        Box::new(|fp| width % fp.w == 0 && height % fp.h == 1),
        Box::new(|fp| width % fp.w == 0 || height % fp.h == 0),
    ];

    //
    // Evaluate each test, from best to worst, against the FactorPairs which are arranged in
    // increasing aspect ratio. Break when we find the best fit. Note the last test and the
    // FactorPair with the highest aspect ratio form a base case since 1 divides anything.
    //
    let mut best_fit = factor_pairs[0];
    'outer: for test in tests.iter() {
        for fp in factor_pairs.iter().rev() {
            if test(fp) {
                best_fit = *fp;
                break 'outer;
            }
        }
    }

    let sz = encoder::Size::new(width / best_fit.w, height / best_fit.h);

    let mut partitions = vec![
        encoder::Rect {
            size: sz,
            origin: euclid::Point2D::new(0, 0)
        };
        best_fit.h * best_fit.w
    ];

    let mut i = 0;
    for y in 0..best_fit.h {
        for x in 0..best_fit.w {
            partitions[i].origin.x = x * sz.width;
            partitions[i].origin.y = y * sz.height;
            i += 1;
        }
    }

    let server_width = partitions
        .iter()
        .max_by_key(|rect| rect.origin.x)
        .map(|rect| rect.origin.x)
        .expect("partition server width")
        + partitions[0].size.width;
    let server_height = partitions
        .iter()
        .max_by_key(|rect| rect.origin.y)
        .map(|rect| rect.origin.y)
        .expect("partition server width")
        + partitions[0].size.height;
    let server_resolution = encoder::Size::new(server_width, server_height);
    (server_resolution, partitions)
}

impl<B: Backend + 'static> AsyncTryTransitionable<BackendInitialized<B>, Created>
    for Server<Created>
{
    type SuccessStateful = Server<BackendInitialized<B>>;
    type FailureStateful = Server<Created>;
    type Error = anyhow::Error;

    async fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let backend = try_recover!(
            B::initialize(
                self.config.max_width,
                self.config.max_height,
                self.state.root_process_cmd.as_mut(),
            )
            .await,
            self
        );
        Ok(Server {
            state: BackendInitialized {
                backend,
                tls_material: self.state.tls_material,
            },
            config: self.config,
        })
    }
}

impl<B: Backend + 'static> AsyncTryTransitionable<Listening<B>, BackendInitialized<B>>
    for Server<BackendInitialized<B>>
{
    type SuccessStateful = Server<Listening<B>>;
    type FailureStateful = Server<BackendInitialized<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
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
            state: Listening {
                backend: self.state.backend,
                channel_server,
            },
            config: self.config,
        })
    }
}

impl<B: Backend + 'static> AsyncTryTransitionable<Accepted<B>, Listening<B>>
    for Server<Listening<B>>
{
    type SuccessStateful = Server<Accepted<B>>;
    type FailureStateful = Server<Listening<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let channel_server = try_transition_inner_recover_async!(
            self.state.channel_server,
            channel_states::AsyncListening,
            |channel_server| async {
                Server {
                    state: Listening {
                        backend: self.state.backend,
                        channel_server,
                    },
                    config: self.config,
                }
            }
        );
        Ok(Server {
            state: Accepted {
                backend: self.state.backend,
                channel_server,
            },
            config: self.config,
        })
    }
}

impl<B: Backend + 'static> Transitionable<Listening<B>> for Server<Accepted<B>> {
    type NextStateful = Server<Listening<B>>;

    fn transition(self) -> Self::NextStateful {
        let channel_server = transition!(self.state.channel_server, channel_states::AsyncListening);

        Server {
            state: Listening {
                backend: self.state.backend,
                channel_server,
            },
            config: self.config,
        }
    }
}

impl<B: Backend + 'static> AsyncTryTransitionable<AuthenticatedClient<B>, Listening<B>>
    for Server<Accepted<B>>
{
    type SuccessStateful = Server<AuthenticatedClient<B>>;
    type FailureStateful = Server<Listening<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        macro_rules! recover_self {
            ($cc:expr, $ec:expr, $mc:expr, $listener:expr) => {{
                recover_self!($cc, $ec, $mc, $listener, self.state.backend)
            }};
            ($cc:expr, $ec:expr, $mc:expr, $listener:expr, $backend: expr) => {{
                let server: channel::Server<channel_states::AsyncReady> =
                    AsyncUnifiedServer::unsplit($cc, $ec, $mc, $listener);
                Server {
                    state: Listening {
                        backend: $backend,
                        channel_server: transition!(server, channel_states::AsyncListening),
                    },
                    config: self.config,
                }
            }};
        }

        let local_addr = try_recover!(self.state.channel_server.local_addr(), self);

        let (mut cc, ec, mc, listener): (channel::AsyncServerControl, _, _, _) =
            try_transition_inner_recover_async!(
                self.state.channel_server,
                channel_states::AsyncReady,
                channel_states::AsyncAccepted,
                |channel_server: channel::Server<channel_states::AsyncAccepted>| future::ready(
                    Server {
                        state: Listening {
                            backend: self.state.backend,
                            channel_server: transition!(
                                channel_server,
                                channel_states::AsyncListening
                            ),
                        },
                        config: self.config
                    }
                )
            )
            .split();

        let msg = try_recover_async!(cc.receive(), recover_self!(cc, ec, mc, listener));
        let client_init = match msg {
            cm_c2s::Message::ClientInit(client_init) => client_init,
            _ => return_recover_async!(
                recover_self!(cc, ec, mc, listener),
                "non client init message received"
            ),
        };
        log::trace!("Client init: {:#?}", client_init);
        if client_init.temp_id != self.config.temp_client_id {
            let msg = cm::ServerGoodbye::from(cm::ServerGoodbyeReason::AuthenticationFailure);
            try_recover_async!(cc.send(msg.into()), recover_self!(cc, ec, mc, listener));
            return_recover_async!(
                recover_self!(cc, ec, mc, listener),
                "mismatched temporary ID"
            );
        }
        if client_init.max_decoder_count < 1 {
            let msg = cm::ServerGoodbye::from(cm::ServerGoodbyeReason::InvalidConfiguration);
            try_recover_async!(cc.send(msg.into()), recover_self!(cc, ec, mc, listener));
            return_recover_async!(
                recover_self!(cc, ec, mc, listener),
                "client sent invalid decoder max"
            );
        }
        let client_recv_buffer_size: usize = match client_init
            .client_info
            .as_ref()
            .and_then(|ci| ci.recv_buffer_size.try_into().ok())
        {
            Some(recv_buffer_size) => recv_buffer_size,
            _ => return_recover!(
                recover_self!(cc, ec, mc, listener),
                "Client did not provide valid recv buffer size"
            ),
        };
        let client_mbps_max: usize = match client_init.mbps_max.try_into().ok() {
            Some(mbps_max) => mbps_max,
            _ => return_recover!(
                recover_self!(cc, ec, mc, listener),
                "Client provided invalid mbps max"
            ),
        };
        let client_resolution = match client_init.client_info.and_then(|ci| ci.display_size) {
            Some(display_size) => display_size,
            _ => return_recover!(
                recover_self!(cc, ec, mc, listener),
                "Client did not provide display size"
            ),
        };

        let mut backend = SwapableBackend::new(self.state.backend);
        if !client_init.client_specified_program_cmdline.is_empty() {
            let cmdline = client_init.client_specified_program_cmdline;

            if !self.config.allow_client_exec {
                let msg = cm::ServerGoodbye::from(cm::ServerGoodbyeReason::ClientExecDisallowed);
                try_recover!(
                    cc.send(msg.into()).await,
                    recover_self!(cc, ec, mc, listener, backend.into_inner().0)
                );
                return_recover!(
                    recover_self!(cc, ec, mc, listener, backend.into_inner().0),
                    "Client attempted to exec '{}', but client exec is disallowed",
                    cmdline
                );
            }

            let client_backend = try_recover!(
                future::ready(parse_create_command(&cmdline))
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
                    log::error!("Failed to launch process");
                    let msg = cm::ServerGoodbye::from(cm::ServerGoodbyeReason::ProcessLaunchFailed);
                    cc.send(msg.clone().into()).await.unwrap_or_else(|e| {
                        log::error!("Failed to send server goodbye {:?}: {}", msg, e)
                    });
                    recover_self!(cc, ec, mc, listener, backend.into_inner().0)
                }
                .await
            );
            backend.set_client_specified(client_backend);
        }

        let (resolution, partitions) = partition(
            &client_resolution,
            client_init.max_decoder_count.try_into().unwrap(),
        );

        let preferred_codec = match client_init.client_caps {
            Some(caps) => {
                if caps.supported_codecs.contains(&Codec::Zlib.into()) {
                    Codec::Zlib
                } else {
                    Codec::Raw
                }
            }
            None => Codec::Raw,
        };

        let decoders: BTreeMap<_, _> = partitions
            .into_iter()
            .enumerate()
            .map(|(id, rect)| {
                (
                    id as u32,
                    (
                        Location::new(rect.origin.x as u64, rect.origin.y as u64),
                        Dimension::new(rect.size.width as u64, rect.size.height as u64),
                        preferred_codec,
                    ),
                )
            })
            .collect();
        try_recover_async!(
            backend.set_resolution(&resolution),
            recover_self!(cc, ec, mc, listener, backend.into_inner().0)
        );
        log::trace!("Resolution set to {:?}", resolution);

        let decoder_areas: Vec<_> = decoders
            .iter()
            .map(|(id, (location, dimension, ..))| cm::DecoderArea {
                decoder_id: *id,
                location: Some(location.clone()),
                dimension: Some(dimension.clone()),
            })
            .collect();
        let client_id: u64 = rand::random();

        let fc_config = flow_control::Configuration::new(
            self.config.window_size,
            self.config.mbps_max,
            local_addr,
            client_mbps_max,
            client_recv_buffer_size,
        );

        let server_init = try_recover!(
            cm::ServerInitBuilder::default()
                .client_id(client_id)
                .server_name(self.config.name.clone())
                .decoder_areas(decoder_areas)
                .display_size(Dimension::new(
                    resolution.width as u64,
                    resolution.height as u64,
                ))
                .window_size(fc_config.get_initial_window_size())
                .build(),
            recover_self!(cc, ec, mc, listener, backend.into_inner().0)
        );
        log::trace!("Server init: {:#?}", server_init);

        try_recover_async!(
            cc.send(server_init.into()),
            recover_self!(cc, ec, mc, listener, backend.into_inner().0)
        );

        let client_hb_interval = try_recover!(
            client_init
                .client_heartbeat_interval
                .map(|d| {
                    Duration::from_secs(d.seconds as u64) + Duration::from_nanos(d.nanos as u64)
                })
                .ok_or(anyhow!("No client HB interval")),
            recover_self!(cc, ec, mc, listener, backend.into_inner().0)
        );
        let client_hb_response_interval = try_recover!(
            client_init
                .client_heartbeat_response_interval
                .map(|d| {
                    Duration::from_secs(d.seconds as u64) + Duration::from_nanos(d.nanos as u64)
                })
                .ok_or(anyhow!("No client HB response interval")),
            recover_self!(cc, ec, mc, listener, backend.into_inner().0)
        );
        Ok(Server {
            state: AuthenticatedClient {
                backend,
                cc,
                ec,
                mc,
                channel_server: listener,
                client_hb_interval,
                client_hb_response_interval,
                decoders,
                fc_config,
            },
            config: self.config,
        })
    }
}

impl<B: Backend + 'static> Transitionable<Listening<B>> for Server<AuthenticatedClient<B>> {
    type NextStateful = Server<Listening<B>>;

    fn transition(self) -> Self::NextStateful {
        Server {
            state: Listening {
                backend: self.state.backend.into_root(),
                channel_server: self.state.channel_server,
            },
            config: self.config,
        }
    }
}

impl<B: Backend + 'static> AsyncTryTransitionable<Running<B>, Listening<B>>
    for Server<AuthenticatedClient<B>>
{
    type SuccessStateful = Server<Running<B>>;
    type FailureStateful = Server<Listening<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let (cc_handler_task, cc_handler_channels) = cc_handler::Task::new(self.state.cc);
        let (flow_control_task, fc_handle) =
            flow_control::Task::new(self.state.fc_config, cc_handler_channels.flow_control_rx);
        let (ec_handler_task, ec_handler_channels) = ec_handler::Task::new(self.state.ec);
        let (mc_handler_task, mc_handler_channels) =
            mc_handler::Task::new(self.state.mc.gated(fc_handle), self.state.decoders.len());

        let (backend_task, backend_channels) = backend::Task::<backend::Created<B>>::new(
            self.state.backend,
            self.state.decoders.clone(),
            cc_handler_channels.missed_frame_rx,
            ec_handler_channels.event_rx,
            ec_handler_channels.to_send_tx.clone(),
            mc_handler_channels.fbu_tx,
            mc_handler_channels.synthetic_missed_frame_rxs,
        );
        let heartbeat_task = heartbeat::Task::<heartbeat::Created>::new(
            self.state.client_hb_interval,
            self.state.client_hb_response_interval,
            backend_channels.acked_seq_tx.clone(),
            cc_handler_channels.hb_resp_rx,
            cc_handler_channels.to_send_tx.clone(),
        );

        macro_rules! recover {
            () => {{
                |_| {
                    future::ready(Server {
                        state: Listening {
                            backend: backend_task.into_backend().into_root(),
                            channel_server: self.state.channel_server,
                        },
                        config: self.config,
                    })
                }
            }};
        }

        let heartbeat_task = try_transition_inner_recover_async!(heartbeat_task, recover!());
        let cc_handler_task = try_transition_inner_recover_async!(cc_handler_task, recover!());
        let ec_handler_task = try_transition_inner_recover_async!(ec_handler_task, recover!());
        let mc_handler_task = try_transition_inner_recover_async!(mc_handler_task, recover!());
        let flow_control_task = try_transition_inner_recover_async!(flow_control_task, recover!());
        let backend_task =
            try_transition_inner_recover_async!(backend_task, |be_task: backend::Task<
                backend::Created<B>,
            >| future::ready(
                Server {
                    state: Listening {
                        backend: be_task.into_backend().into_root(),
                        channel_server: self.state.channel_server
                    },
                    config: self.config
                }
            ));

        Ok(Server {
            state: Running {
                ct: CancellationToken::new(),
                channel_server: self.state.channel_server,
                cc_tx: cc_handler_channels.to_send_tx.clone(),
                backend_task,
                heartbeat_task,
                cc_handler_task,
                ec_handler_task,
                mc_handler_task,
                flow_control_task,
            },
            config: self.config,
        })
    }
}

impl<B: Backend + 'static> AsyncTryTransitionable<SessionComplete<B>, SessionComplete<B>>
    for Server<Running<B>>
{
    type SuccessStateful = Server<SessionComplete<B>>;
    type FailureStateful = Server<SessionComplete<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let mut top_level_cleanup_executed = false;
        let mut has_sent_gb = false;
        let mut server_gb_reason: Option<cm::ServerGoodbyeReason> = None;

        let mut backend_terminated = None;
        let mut heartbeat_terminated = None;
        let mut cc_handler_terminated = None;
        let mut ec_handler_terminated = None;
        let mut mc_handler_terminated = None;
        let mut flow_control_terminated = None;

        let heartbeat_ct = self.state.heartbeat_task.cancellation_token().clone();
        let backend_ct = self.state.backend_task.cancellation_token().clone();
        let cc_handler_ct = self.state.cc_handler_task.cancellation_token().clone();
        let ec_handler_ct = self.state.ec_handler_task.cancellation_token().clone();
        let mc_handler_ct = self.state.mc_handler_task.cancellation_token().clone();
        let flow_control_ct = self.state.flow_control_task.cancellation_token().clone();
        let client_process_ct = CancellationToken::new();

        let mut heartbeat_task = tokio::spawn(async move {
            try_transition_async!(self.state.heartbeat_task, heartbeat::Terminated)
        });
        let mut backend_task = tokio::spawn(async move {
            try_transition_async!(self.state.backend_task, backend::Terminated<B>)
        });
        let mut cc_handler_task = tokio::spawn(async move {
            try_transition_async!(self.state.cc_handler_task, cc_handler::Terminated)
        });
        let mut ec_handler_task = tokio::spawn(async move {
            try_transition_async!(self.state.ec_handler_task, ec_handler::Terminated)
        });
        let mut mc_handler_task = tokio::spawn(async move {
            try_transition_async!(self.state.mc_handler_task, mc_handler::Terminated)
        });
        let mut flow_control_task = tokio::spawn(async move {
            try_transition_async!(self.state.flow_control_task, flow_control::Terminated)
        });

        loop {
            tokio::select! {
                biased;
                _ = self.state.ct.cancelled(), if !top_level_cleanup_executed => {
                    log::debug!("Server task is cancelled");
                    top_level_cleanup_executed = true;
                    heartbeat_ct.cancel();
                    backend_ct.cancel();
                    cc_handler_ct.cancel();
                    ec_handler_ct.cancel();
                    mc_handler_ct.cancel();
                    flow_control_ct.cancel();
                    client_process_ct.cancel();
                }
                _ = self.state.ct.cancelled(), if server_gb_reason.is_some() && !has_sent_gb => {
                    if let Some(reason) = server_gb_reason {
                        log::debug!("Sending server goodbye: {:?}", reason);
                        let msg = cm::ServerGoodbye::from(reason).into();
                        self.state.cc_tx.send(msg).await
                            .unwrap_or_else(|e| log::error!("Failed to send server goodbye: {}", e));
                    }
                    has_sent_gb = true;
                    cc_handler_ct.cancel();
                }
                be_term = &mut backend_task, if backend_terminated.is_none() => {
                    log::debug!("Backend subtask completed");
                    if server_gb_reason.is_none() {
                        server_gb_reason = match &be_term {
                            Ok(Err(_)) | Err(_) => Some(cm::ServerGoodbyeReason::InternalError),
                            Ok(Ok(be_task_terminated)) => if be_task_terminated.root_process_exited() {
                                Some(cm::ServerGoodbyeReason::ProcessExited)
                            } else {
                                None
                            }
                        };
                    }
                    backend_terminated = Some(be_term);
                    self.state.ct.cancel();
                }
                hb_term = &mut heartbeat_task, if heartbeat_terminated.is_none() => {
                    log::debug!("Heartbeat subtask completed");
                    if server_gb_reason.is_none() && hb_term.is_err() {
                        server_gb_reason = Some(cm::ServerGoodbyeReason::NetworkError);
                    }
                    heartbeat_terminated = Some(hb_term);
                    self.state.ct.cancel();
                }
                cc_term = &mut cc_handler_task, if cc_handler_terminated.is_none() => {
                    log::debug!("CC subtask completed");
                    if server_gb_reason.is_none() && cc_term.is_err() {
                        server_gb_reason = Some(cm::ServerGoodbyeReason::InternalError);
                    }
                    cc_handler_terminated = Some(cc_term);
                    self.state.ct.cancel();
                }
                ec_term = &mut ec_handler_task, if ec_handler_terminated.is_none() => {
                    log::debug!("EC subtask completed");
                    if server_gb_reason.is_none() && ec_term.is_err() {
                        server_gb_reason = Some(cm::ServerGoodbyeReason::InternalError);
                    }
                    ec_handler_terminated = Some(ec_term);
                    self.state.ct.cancel();
                }
                mc_term = &mut mc_handler_task, if mc_handler_terminated.is_none() => {
                    log::debug!("MC subtask completed");
                    if server_gb_reason.is_none() && mc_term.is_err() {
                        server_gb_reason = Some(cm::ServerGoodbyeReason::InternalError);
                    }
                    mc_handler_terminated = Some(mc_term);
                    self.state.ct.cancel();
                }
                fc_term = &mut flow_control_task, if flow_control_terminated.is_none() => {
                    log::debug!("Flow Control subtask completed");
                    if server_gb_reason.is_none() && fc_term.is_err() {
                        server_gb_reason = Some(cm::ServerGoodbyeReason::InternalError);
                    }
                    flow_control_terminated = Some(fc_term);
                    self.state.ct.cancel();
                }
                else => break
            }
        }
        let backend_terminated = backend_terminated.expect("backend terminated");
        let heartbeat_terminated = heartbeat_terminated.expect("heartbeat terminated");
        let cc_handler_terminated = cc_handler_terminated.expect("cc handler terminated");
        let ec_handler_terminated = ec_handler_terminated.expect("ec handler terminated");
        let mc_handler_terminated = mc_handler_terminated.expect("mc handler terminated");
        let flow_control_terminated = flow_control_terminated.expect("flow control terminated");

        let mut errors = vec![];
        let backend = match backend_terminated {
            Ok(Ok(backend_terminated)) => backend_terminated,
            Ok(Err(Recovered { error, stateful })) => {
                log::error!("Backend terminated with error: {}", error);
                errors.push(error);
                stateful
            }
            Err(join_error) => {
                panic!(
                    "Backend task panicked leaving backend unrecoverable: {}",
                    join_error
                );
            }
        }
        .into_backend();

        if let Err(e) = heartbeat_terminated {
            errors.push(e.into());
        }
        if let Err(e) = cc_handler_terminated {
            errors.push(e.into());
        }
        if let Err(e) = ec_handler_terminated {
            errors.push(e.into());
        }
        if let Err(e) = mc_handler_terminated {
            errors.push(e.into());
        }
        if let Err(e) = flow_control_terminated {
            errors.push(e.into());
        }

        let stateful = Server {
            state: SessionComplete {
                backend,
                channel_server: self.state.channel_server,
            },
            config: self.config,
        };
        if !errors.is_empty() {
            let error = anyhow!(
                "Underlying server tasks failed with errors: {}",
                errors
                    .into_iter()
                    .map(|err| format!("{}", err))
                    .collect::<Vec<_>>()
                    .join("\t\n")
            );
            Err(Recovered::new(stateful, error))
        } else {
            Ok(stateful)
        }
    }
}

impl<B: Backend + 'static> AsyncTransitionable<SessionComplete<B>> for Server<Running<B>> {
    type NextStateful = Server<SessionComplete<B>>;

    async fn transition(self) -> Self::NextStateful {
        self.stop();

        let backend =
            transition_async!(self.state.backend_task, backend::Terminated<B>).into_backend();
        Server {
            state: SessionComplete {
                backend,
                channel_server: self.state.channel_server,
            },
            config: self.config,
        }
    }
}

impl<B: Backend + 'static> Transitionable<Listening<B>> for Server<SessionComplete<B>> {
    type NextStateful = Server<Listening<B>>;

    fn transition(self) -> Self::NextStateful {
        Server {
            state: Listening {
                backend: self.state.backend.into_root(),
                channel_server: self.state.channel_server,
            },
            config: self.config,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::backend::X;

    use aperturec_channel::{
        tls, AsyncClientControl, AsyncClientEvent, AsyncClientMedia, AsyncUnifiedClient,
    };
    use aperturec_protocol::control::*;

    use rand::Rng;

    fn client_init_msg(id: u64) -> cm_c2s::Message {
        ClientInitBuilder::default()
            .temp_id(id)
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
            .client_heartbeat_interval::<prost_types::Duration>(
                Duration::from_millis(1000_u64).try_into().unwrap(),
            )
            .client_heartbeat_response_interval::<prost_types::Duration>(
                Duration::from_millis(1000_u64).try_into().unwrap(),
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
        u64,
    ) {
        let tlsdir = tempdir::TempDir::new("").expect("temp dir");
        let client_id = rand::thread_rng().gen();
        let server_config = ConfigurationBuilder::default()
            .bind_addr("127.0.0.1:0".to_string())
            .name("test server".into())
            .tls_configuration(TlsConfiguration::Generated {
                save_directory: tlsdir.path().to_path_buf(),
                external_addresses: vec![],
            })
            .temp_client_id(client_id)
            .max_width(1920)
            .max_height(1080)
            .allow_client_exec(false)
            .mbps_max(500.into())
            .window_size(None)
            .build()
            .expect("Configuration build");
        let server: Server<Listening<X>> = Server::new(server_config.clone())
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
        let (cc, ec, mc) = client.split();
        (server, cc, ec, mc, client_id)
    }

    async fn server_client_authenticated() -> (
        Server<AuthenticatedClient<X>>,
        AsyncClientControl,
        AsyncClientEvent,
        AsyncClientMedia,
    ) {
        let (server, mut client_cc, client_ec, client_mc, client_id) =
            server_client_connected().await;
        client_cc
            .send(client_init_msg(client_id))
            .await
            .expect("send ClientInit");
        let server = server.try_transition().await.expect("failed to auth");
        (server, client_cc, client_ec, client_mc)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auth_fail() {
        let (server, mut client_cc, ..) = server_client_connected().await;
        client_cc
            .send(client_init_msg(5678))
            .await
            .expect("send ClientInit");
        try_transition_async!(server, AuthenticatedClient<X>).expect_err("client authenticate");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auth_pass() {
        let _ = server_client_authenticated().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn client_init() {
        let (_, mut client_cc, ..) = server_client_authenticated().await;
        let msg = client_cc.receive().await.expect("receiving server init");
        let server_init = if let cm_s2c::Message::ServerInit(server_init) = msg {
            assert_eq!(server_init.decoder_areas.len(), 2);
            server_init
        } else {
            panic!("non server init message");
        };

        assert_eq!(server_init.decoder_areas.len(), 2);
        assert_eq!(
            server_init.decoder_areas[0]
                .dimension
                .as_ref()
                .expect("dimension")
                .width,
            512
        );
        assert_eq!(
            server_init.decoder_areas[1]
                .dimension
                .as_ref()
                .expect("dimension")
                .width,
            512
        );
        assert_eq!(
            server_init.decoder_areas[0]
                .dimension
                .as_ref()
                .expect("dimension")
                .height,
            768
        );
        assert_eq!(
            server_init.decoder_areas[1]
                .dimension
                .as_ref()
                .expect("dimension")
                .height,
            768
        );
        assert_eq!(
            server_init.decoder_areas[0]
                .location
                .as_ref()
                .expect("location")
                .x_position,
            0
        );
        assert_eq!(
            server_init.decoder_areas[1]
                .location
                .as_ref()
                .expect("location")
                .x_position,
            512
        );
        assert_eq!(
            server_init.decoder_areas[0]
                .location
                .as_ref()
                .expect("location")
                .y_position,
            0
        );
        assert_eq!(
            server_init.decoder_areas[1]
                .location
                .as_ref()
                .expect("location")
                .y_position,
            0
        );
        assert_eq!(
            server_init
                .display_size
                .as_ref()
                .expect("display_size")
                .width,
            1024
        );
        assert_eq!(
            server_init
                .display_size
                .as_ref()
                .expect("display_size")
                .height,
            768
        );
    }

    #[test]
    fn partitions() {
        let mut dims = vec![];

        dims.push((Dimension::new(800, 600), 8));
        dims.push((Dimension::new(1470, 956), 8));
        dims.push((Dimension::new(813, 600), 32));

        for d in dims.iter() {
            let (_, partitions) = partition(&d.0, d.1);
            for p in partitions.iter() {
                assert!(
                    p.origin.x == 0 || (p.origin.x % p.width()) == 0,
                    "Invalid x/width {:#?}",
                    p,
                );
                assert!(
                    p.origin.y == 0 || (p.origin.y % p.height()) == 0,
                    "Invalid y/height {:#?}",
                    p,
                );
            }
        }
    }
}
