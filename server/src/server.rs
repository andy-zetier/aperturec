use crate::backend::Backend;
use crate::task::{
    backend, control_channel_handler as cc_handler, encoder, event_channel_handler as ec_handler,
    heartbeat,
};

use anyhow::{anyhow, Result};
use aperturec_channel::codec::unreliable::ReceiverSimplex;
use aperturec_channel::reliable::tcp;
use aperturec_channel::unreliable::udp;
use aperturec_channel::*;
use aperturec_protocol::common::*;
use aperturec_protocol::control as cm;
use aperturec_protocol::control::client_to_server as cm_c2s;
use aperturec_protocol::control::server_to_client as cm_s2c;
use aperturec_protocol::media::client_to_server as mm_c2s;
use aperturec_protocol::*;
use aperturec_state_machine::*;
use aperturec_trace::log;
use async_trait::async_trait;
use derive_builder::Builder;
use std::cmp::{max, min};
use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Builder, Debug, Clone)]
pub struct Configuration {
    control_channel_addr: SocketAddr,
    event_channel_addr: SocketAddr,
    media_channel_addr: SocketAddr,
    name: String,
    temp_client_id: u64,
    max_width: usize,
    max_height: usize,
    #[builder(setter(strip_option), default)]
    default_root_program: Option<String>,
}

#[derive(Stateful, Debug)]
#[state(S)]
pub struct Server<S: State> {
    state: S,
    config: Configuration,
}

#[derive(State, Debug)]
pub struct Created;
impl SelfTransitionable for Server<Created> {}

#[derive(State, Debug)]
pub struct BackendInitialized<B: Backend + 'static> {
    backend: B,
}
impl<B: Backend + 'static> SelfTransitionable for Server<BackendInitialized<B>> {}

#[derive(State, Debug)]
pub struct ChannelsListening<B: Backend + 'static> {
    backend: B,
    cc_server: tcp::Server<tcp::Listening>,
    ec_server: tcp::Server<tcp::Listening>,
}
impl<B: Backend + 'static> SelfTransitionable for Server<ChannelsListening<B>> {}

#[derive(State)]
pub struct ControlChannelAccepted<B: Backend + 'static> {
    backend: B,
    cc: AsyncServerControlChannel,
    ec_server: tcp::Server<tcp::Listening>,
}

#[derive(State)]
pub struct AuthenticatedClient<B: Backend + 'static> {
    backend: B,
    cc: AsyncServerControlChannel,
    ec_server: tcp::Server<tcp::Listening>,
    client_hb_interval: Duration,
    client_hb_response_interval: Duration,
    decoders: Vec<(Decoder, Location, Dimension)>,
    mc_servers: Vec<udp::Server<udp::Listening>>,
    codecs: BTreeMap<u16, Codec>,
}

#[derive(State)]
pub struct ChannelsAccepted<B: Backend + 'static> {
    backend: B,
    cc: AsyncServerControlChannel,
    ec: AsyncServerEventChannel,
    client_hb_interval: Duration,
    client_hb_response_interval: Duration,
    decoders: Vec<(Decoder, Location, Dimension)>,
    mc_servers: Vec<udp::Server<udp::Connected>>,
    codecs: BTreeMap<u16, Codec>,
}

#[derive(State)]
pub struct Running<B: Backend + 'static> {
    ct: CancellationToken,
    cc_tx: mpsc::Sender<cm_s2c::Message>,
    backend_task: backend::Task<backend::Running<B>>,
    heartbeat_task: heartbeat::Task<heartbeat::Running>,
    cc_handler_task: cc_handler::Task<cc_handler::Running>,
    ec_handler_task: ec_handler::Task<ec_handler::Running>,
}

#[derive(State)]
pub struct SessionComplete<B: Backend + 'static> {
    backend: B,
    cc_server: tcp::Server<tcp::Listening>,
    ec_server: tcp::Server<tcp::Listening>,
}

impl Server<Created> {
    pub fn new(config: Configuration) -> Result<Self> {
        if let Some(default_root_program) = &config.default_root_program {
            which::which(default_root_program).map_err(|e| {
                anyhow!(
                    "Cannot launch server with default root program '{}': {}",
                    default_root_program,
                    e
                )
            })?;
        }
        Ok(Server {
            state: Created,
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

#[async_trait]
impl<B: Backend + 'static> TryTransitionable<BackendInitialized<B>, Created> for Server<Created> {
    type SuccessStateful = Server<BackendInitialized<B>>;
    type FailureStateful = Server<Created>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        Ok(Server {
            state: BackendInitialized {
                backend: try_recover!(
                    B::initialize(self.config.max_width, self.config.max_height).await,
                    self
                ),
            },
            config: self.config,
        })
    }
}

#[async_trait]
impl<B: Backend + 'static> TryTransitionable<ChannelsListening<B>, BackendInitialized<B>>
    for Server<BackendInitialized<B>>
{
    type SuccessStateful = Server<ChannelsListening<B>>;
    type FailureStateful = Server<BackendInitialized<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let cc_server = tcp::Server::<tcp::Closed>::new(self.config.control_channel_addr);
        let cc_server_listening = try_transition_inner_recover!(cc_server, tcp::Closed, |_| self);
        let ec_server = tcp::Server::<tcp::Closed>::new(self.config.event_channel_addr);
        let ec_server_listening = try_transition_inner_recover!(ec_server, tcp::Closed, |_| self);
        Ok(Server {
            state: ChannelsListening {
                backend: self.state.backend,
                cc_server: cc_server_listening,
                ec_server: ec_server_listening,
            },
            config: self.config,
        })
    }
}

#[async_trait]
impl<B: Backend + 'static> TryTransitionable<ControlChannelAccepted<B>, ChannelsListening<B>>
    for Server<ChannelsListening<B>>
{
    type SuccessStateful = Server<ControlChannelAccepted<B>>;
    type FailureStateful = Server<ChannelsListening<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        macro_rules! recover_cc_server_constructor {
            () => {{
                |recovered_cc_server| Server {
                    state: ChannelsListening {
                        backend: self.state.backend,
                        cc_server: recovered_cc_server,
                        ec_server: self.state.ec_server,
                    },
                    config: self.config,
                }
            }};
        }
        let cc_accepted: tcp::Server<tcp::Accepted> = try_transition_inner_recover!(
            self.state.cc_server,
            tcp::Listening,
            recover_cc_server_constructor!()
        );
        let cc_async: tcp::Server<tcp::AsyncAccepted> = try_transition_inner_recover!(
            cc_accepted,
            tcp::Listening,
            recover_cc_server_constructor!()
        );
        let cc = AsyncServerControlChannel::new(cc_async);

        Ok(Server {
            state: ControlChannelAccepted {
                backend: self.state.backend,
                cc,
                ec_server: self.state.ec_server,
            },
            config: self.config,
        })
    }
}

impl<B: Backend + 'static> Transitionable<ChannelsListening<B>>
    for Server<ControlChannelAccepted<B>>
{
    type NextStateful = Server<ChannelsListening<B>>;

    fn transition(self) -> Self::NextStateful {
        let cc_listening = transition!(self.state.cc.into_inner(), tcp::Listening);

        Server {
            state: ChannelsListening {
                backend: self.state.backend,
                cc_server: cc_listening,
                ec_server: self.state.ec_server,
            },
            config: self.config,
        }
    }
}

#[async_trait]
impl<B: Backend + 'static> TryTransitionable<AuthenticatedClient<B>, ChannelsListening<B>>
    for Server<ControlChannelAccepted<B>>
{
    type SuccessStateful = Server<AuthenticatedClient<B>>;
    type FailureStateful = Server<ChannelsListening<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let msg = try_recover!(self.state.cc.receive().await, self);
        let client_init = match msg {
            cm_c2s::Message::ClientInit(client_init) => client_init,
            _ => return_recover!(self, "non client init message received"),
        };
        log::trace!("Client init: {:#?}", client_init);
        if client_init.temp_id != self.config.temp_client_id {
            let msg = cm::ServerGoodbye::from(cm::ServerGoodbyeReason::AuthenticationFailure);
            try_recover!(self.state.cc.send(msg.into()).await, self);
            return_recover!(self, "mismatched temporary ID");
        }
        if client_init.max_decoder_count < 1 {
            let msg = cm::ServerGoodbye::from(cm::ServerGoodbyeReason::InvalidConfiguration);
            try_recover!(self.state.cc.send(msg.into()).await, self);
            return_recover!(self, "client sent invalid decoder max");
        }

        let (root_process, should_replace) = if client_init.root_program.is_empty() {
            (self.config.default_root_program.as_ref(), false)
        } else {
            (Some(&client_init.root_program), true)
        };
        if let Some(root_process) = root_process {
            try_recover!(
                self.state
                    .backend
                    .start_root_process(root_process, should_replace)
                    .await,
                (|| async move {
                    log::error!("Failed to launch root process");
                    let msg =
                        cm::ServerGoodbye::from(cm::ServerGoodbyeReason::RootProcessLaunchFailed);
                    self.state
                        .cc
                        .send(msg.clone().into())
                        .await
                        .unwrap_or_else(|e| {
                            log::error!("Failed to send server goodbye {:?}: {}", msg, e)
                        });
                    self
                })()
                .await
            );
        }

        let client_resolution = match client_init.client_info.and_then(|ci| ci.display_size) {
            Some(display_size) => display_size,
            _ => return_recover!(self, "Client did not provide display size"),
        };

        let (resolution, partitions) = partition(
            &client_resolution,
            client_init.max_decoder_count.try_into().unwrap(),
        );

        let mut decoders: Vec<(Decoder, Location, Dimension)> = vec![];
        let mut mc_servers: Vec<udp::Server<udp::Listening>> = vec![];
        let port_start = self.config.media_channel_addr.port();
        let mut local_mc_addr = self.config.media_channel_addr;

        for (i, rect) in partitions.into_iter().enumerate() {
            if port_start != 0 {
                local_mc_addr.set_port(port_start + i as u16);
            }
            let mc_server = udp::Server::<udp::Closed>::new(local_mc_addr);
            let mc_server_listening = try_transition_inner_recover!(mc_server, udp::Closed, |_| {
                Server {
                    state: ChannelsListening {
                        backend: self.state.backend,
                        cc_server: self.state.cc.into_inner().transition(),
                        ec_server: self.state.ec_server,
                    },
                    config: self.config,
                }
            });

            decoders.push((
                Decoder::new(mc_server_listening.local_addr().port() as u32),
                Location::new(rect.origin.x as u64, rect.origin.y as u64),
                Dimension::new(rect.size.width as u64, rect.size.height as u64),
            ));
            mc_servers.push(mc_server_listening);
        }

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

        let codecs = decoders
            .iter()
            .map(|(decoder, _, _)| (decoder.port as u16, preferred_codec))
            .collect();
        try_recover!(self.state.backend.set_resolution(&resolution).await, self);
        log::trace!("Resolution set to {:?}", resolution);
        let cursor_bitmaps = try_recover!(self.state.backend.cursor_bitmaps().await, self);

        let decoder_areas: Vec<_> = decoders
            .iter()
            .map(|(decoder, location, dimension)| cm::DecoderArea {
                decoder: Some(decoder.clone()),
                location: Some(location.clone()),
                dimension: Some(dimension.clone()),
            })
            .collect();
        let client_id: u64 = rand::random();
        let server_init = try_recover!(
            cm::ServerInitBuilder::default()
                .client_id(client_id)
                .server_name(self.config.name.clone())
                .cursor_bitmaps(cursor_bitmaps)
                .decoder_areas(decoder_areas)
                .event_port(self.config.event_channel_addr.port())
                .display_size(Dimension::new(
                    resolution.width as u64,
                    resolution.height as u64
                ))
                .build(),
            self
        );
        log::trace!("Server init: {:#?}", server_init);

        try_recover!(self.state.cc.send(server_init.into()).await, self);

        let client_hb_interval = try_recover!(
            client_init
                .client_heartbeat_interval
                .map(|d| Duration::from_secs(d.seconds as u64)
                    + Duration::from_nanos(d.nanos as u64))
                .ok_or(anyhow!("No client HB interval")),
            self
        );
        let client_hb_response_interval = try_recover!(
            client_init
                .client_heartbeat_response_interval
                .map(|d| Duration::from_secs(d.seconds as u64)
                    + Duration::from_nanos(d.nanos as u64))
                .ok_or(anyhow!("No client HB response interval")),
            self
        );
        Ok(Server {
            state: AuthenticatedClient {
                backend: self.state.backend,
                cc: self.state.cc,
                ec_server: self.state.ec_server,
                client_hb_interval,
                client_hb_response_interval,
                decoders,
                mc_servers,
                codecs,
            },
            config: self.config,
        })
    }
}

impl<B: Backend + 'static> Transitionable<ChannelsListening<B>> for Server<AuthenticatedClient<B>> {
    type NextStateful = Server<ChannelsListening<B>>;

    fn transition(self) -> Self::NextStateful {
        let cc_server = transition!(self.state.cc.into_inner(), tcp::Listening);
        Server {
            state: ChannelsListening {
                backend: self.state.backend,
                cc_server,
                ec_server: self.state.ec_server,
            },
            config: self.config,
        }
    }
}

#[async_trait]
impl<B: Backend + 'static> TryTransitionable<ChannelsAccepted<B>, ChannelsListening<B>>
    for Server<AuthenticatedClient<B>>
{
    type SuccessStateful = Server<ChannelsAccepted<B>>;
    type FailureStateful = Server<ChannelsListening<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        macro_rules! recover_ec_server_constructor {
            () => {{
                |recovered_ec_server| Server {
                    state: ChannelsListening {
                        backend: self.state.backend,
                        cc_server: transition!(self.state.cc.into_inner(), tcp::Listening),
                        ec_server: recovered_ec_server,
                    },
                    config: self.config,
                }
            }};
        }

        let ec_accepted = try_transition_inner_recover!(
            self.state.ec_server,
            tcp::Listening,
            recover_ec_server_constructor!()
        );
        let ec_async = try_transition_inner_recover!(
            ec_accepted,
            tcp::Listening,
            recover_ec_server_constructor!()
        );

        let ec = AsyncServerEventChannel::new(ec_async);

        macro_rules! recover_mc_server_constructor {
            () => {{
                Server {
                    state: ChannelsListening {
                        backend: self.state.backend,
                        cc_server: transition!(self.state.cc.into_inner(), tcp::Listening),
                        ec_server: transition!(ec.into_inner(), tcp::Listening),
                    },
                    config: self.config,
                }
            }};
        }

        let mut mc_servers: Vec<udp::Server<udp::Connected>> = vec![];

        let pause = Duration::from_millis(250);
        let max_delay = Duration::from_secs(5);
        let mut delay = Duration::from_secs(0);

        for mc_server in self.state.mc_servers {
            let port = mc_server.local_addr().port();
            let mut mc_rs: ReceiverSimplex<
                udp::Server<udp::Listening>,
                mm_c2s::Message,
                media::ClientToServer,
            > = ReceiverSimplex::new(mc_server);

            loop {
                match mc_rs.receive() {
                    Ok(mm_c2s::Message::MediaKeepalive(mk)) => {
                        log::trace!("Received initial {:?}", mk);
                        break;
                    }
                    Err(ref err) => {
                        if err
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|io| io.kind() == std::io::ErrorKind::WouldBlock)
                            && delay < max_delay
                        {
                            tokio::time::sleep(pause).await;
                            delay += pause;
                            log::warn!(
                                "Waited {}.{}s for initial MediaKeepalive from Decoder {}: {:?}",
                                delay.as_secs(),
                                delay.subsec_millis(),
                                port,
                                err
                            );
                            continue;
                        }

                        return_recover!(
                            recover_mc_server_constructor!(),
                            format!(
                                "Failed to receive MediaKeepalive after {}.{}s from Decoder {}: {:?}",
                                delay.as_secs(),
                                delay.subsec_millis(),
                                port,
                                err
                            )
                        );
                    }
                };
            }

            let mc_connected =
                try_transition_inner_recover!(mc_rs.into_inner(), udp::Closed, |_| Server {
                    state: ChannelsListening {
                        backend: self.state.backend,
                        cc_server: transition!(self.state.cc.into_inner(), tcp::Listening),
                        ec_server: transition!(ec.into_inner(), tcp::Listening)
                    },
                    config: self.config,
                });
            mc_servers.push(mc_connected);
        }

        Ok(Server {
            state: ChannelsAccepted {
                backend: self.state.backend,
                cc: self.state.cc,
                ec,
                client_hb_interval: self.state.client_hb_interval,
                client_hb_response_interval: self.state.client_hb_response_interval,
                decoders: self.state.decoders,
                mc_servers,
                codecs: self.state.codecs,
            },
            config: self.config,
        })
    }
}

#[async_trait]
impl<B: Backend + 'static> TryTransitionable<Running<B>, ChannelsListening<B>>
    for Server<ChannelsAccepted<B>>
{
    type SuccessStateful = Server<Running<B>>;
    type FailureStateful = Server<ChannelsListening<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let remote_addr = try_recover!(self.state.cc.as_ref().as_ref().peer_addr(), self);

        let (cc_handler_task, cc_handler_channels) = cc_handler::Task::new(self.state.cc);
        let (ec_handler_task, ec_handler_channels) = ec_handler::Task::new(self.state.ec);

        let (backend_task, backend_channels) = backend::Task::<backend::Created<B>>::new(
            self.state.backend,
            &self.state.decoders,
            self.state.mc_servers,
            self.state.codecs,
            &remote_addr,
            ec_handler_channels.event_rx,
            cc_handler_channels.fb_update_req_rx,
            cc_handler_channels.missed_frame_rx,
        );
        let heartbeat_task = heartbeat::Task::<heartbeat::Created>::new(
            self.state.client_hb_interval,
            self.state.client_hb_response_interval,
            backend_channels.acked_seq_tx.clone(),
            cc_handler_channels.hb_resp_rx,
            cc_handler_channels.to_send_tx.clone(),
        );

        let heartbeat_task =
            try_transition_inner_recover!(heartbeat_task, heartbeat::Created, |_| {
                Server {
                    state: ChannelsListening {
                        backend: backend_task.into_backend(),
                        cc_server: cc_handler_task.into_control_channel_server(),
                        ec_server: ec_handler_task.into_event_channel_server(),
                    },
                    config: self.config,
                }
            });
        let cc_handler_task = try_transition_inner_recover!(
            cc_handler_task,
            cc_handler::Created,
            |recovered: cc_handler::Task<cc_handler::Created>| {
                Server {
                    state: ChannelsListening {
                        backend: backend_task.into_backend(),
                        cc_server: recovered.into_control_channel_server(),
                        ec_server: ec_handler_task.into_event_channel_server(),
                    },
                    config: self.config,
                }
            }
        );
        let ec_handler_task = try_transition_inner_recover!(
            ec_handler_task,
            ec_handler::Created,
            |recovered: ec_handler::Task<ec_handler::Created>| {
                Server {
                    state: ChannelsListening {
                        backend: backend_task.into_backend(),
                        cc_server: transition!(cc_handler_task, cc_handler::Terminated)
                            .into_control_channel_server(),
                        ec_server: recovered.into_event_channel_server(),
                    },
                    config: self.config,
                }
            }
        );
        let backend_task: backend::Task<backend::Running<B>> = try_transition_inner_recover!(
            backend_task,
            backend::Created<B>,
            |recovered: backend::Task<backend::Created<B>>| {
                Server {
                    state: ChannelsListening {
                        backend: recovered.into_backend(),
                        cc_server: transition!(cc_handler_task, cc_handler::Terminated)
                            .into_control_channel_server(),
                        ec_server: transition!(ec_handler_task, ec_handler::Terminated)
                            .into_event_channel_server(),
                    },
                    config: self.config,
                }
            }
        );

        Ok(Server {
            state: Running {
                ct: CancellationToken::new(),
                cc_tx: cc_handler_channels.to_send_tx.clone(),
                backend_task,
                heartbeat_task,
                cc_handler_task,
                ec_handler_task,
            },
            config: self.config,
        })
    }
}

impl<B: Backend + 'static> Transitionable<ChannelsListening<B>> for Server<ChannelsAccepted<B>> {
    type NextStateful = Server<ChannelsListening<B>>;

    fn transition(self) -> Self::NextStateful {
        Server {
            state: ChannelsListening {
                backend: self.state.backend,
                cc_server: transition!(self.state.cc.into_inner(), tcp::Listening),
                ec_server: transition!(self.state.ec.into_inner(), tcp::Listening),
            },
            config: self.config,
        }
    }
}

#[async_trait]
impl<B: Backend + 'static> TryTransitionable<SessionComplete<B>, SessionComplete<B>>
    for Server<Running<B>>
{
    type SuccessStateful = Server<SessionComplete<B>>;
    type FailureStateful = Server<SessionComplete<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let mut backend_terminated = None;
        let mut heartbeat_terminated = None;
        let mut cc_handler_terminated = None;
        let mut ec_handler_terminated = None;

        let heartbeat_ct = self.state.heartbeat_task.cancellation_token().clone();
        let backend_ct = self.state.backend_task.cancellation_token().clone();
        let cc_handler_ct = self.state.cc_handler_task.cancellation_token().clone();
        let ec_handler_ct = self.state.ec_handler_task.cancellation_token().clone();

        let mut backend_task =
            tokio::spawn(async move { self.state.backend_task.try_transition().await });
        let mut heartbeat_task =
            tokio::spawn(async move { self.state.heartbeat_task.try_transition().await });
        let mut cc_handler_task =
            tokio::spawn(async move { self.state.cc_handler_task.try_transition().await });
        let mut ec_handler_task =
            tokio::spawn(async move { self.state.ec_handler_task.try_transition().await });

        let mut top_level_cleanup_executed = false;
        let mut has_sent_gb = false;
        let mut server_gb_reason: Option<cm::ServerGoodbyeReason> = None;
        loop {
            tokio::select! {
                biased;
                _ = self.state.ct.cancelled(), if !top_level_cleanup_executed => {
                    log::debug!("Server task is cancelled");
                    top_level_cleanup_executed = true;
                    heartbeat_ct.cancel();
                    backend_ct.cancel();
                    ec_handler_ct.cancel();
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
                hb_term = &mut heartbeat_task, if heartbeat_terminated.is_none() => {
                    log::debug!("Heartbeat subtask completed");
                    if server_gb_reason.is_none() && hb_term.is_err() {
                        server_gb_reason = Some(cm::ServerGoodbyeReason::NetworkError);
                    }
                    heartbeat_terminated = Some(hb_term);
                    self.state.ct.cancel();
                }
                be_term = &mut backend_task, if backend_terminated.is_none() => {
                    log::debug!("Backend subtask completed");
                    if server_gb_reason.is_none() {
                        server_gb_reason = match &be_term {
                            Ok(Err(_)) | Err(_) => Some(cm::ServerGoodbyeReason::InternalError),
                            Ok(Ok(be_task_terminated)) => if be_task_terminated.root_process_exited() {
                                Some(cm::ServerGoodbyeReason::RootExited)
                            } else {
                                None
                            }
                        };
                    }
                    backend_terminated = Some(be_term);
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
                cc_term = &mut cc_handler_task, if cc_handler_terminated.is_none() => {
                    log::debug!("CC subtask completed");
                    if server_gb_reason.is_none() && cc_term.is_err() {
                        server_gb_reason = Some(cm::ServerGoodbyeReason::InternalError);
                    }
                    cc_handler_terminated = Some(cc_term);
                    self.state.ct.cancel();
                }
                else => break
            }
        }
        let backend_terminated = backend_terminated.expect("backend terminated");
        let heartbeat_terminated = heartbeat_terminated.expect("heartbeat terminated");
        let cc_handler_terminated = cc_handler_terminated.expect("cc handler terminated");
        let ec_handler_terminated = ec_handler_terminated.expect("ec handler terminated");

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

        match heartbeat_terminated {
            Ok(Ok(_)) => (),
            Ok(Err(Recovered { error, .. })) => {
                log::error!("Heartbeat terminated with error: {}", error);
                errors.push(error);
            }
            Err(join_error) => {
                log::error!("Heartbeat task panicked: {}", join_error);
                errors.push(join_error.into());
            }
        }

        let cc_server = match cc_handler_terminated {
            Ok(Ok(cc)) => cc,
            Ok(Err(Recovered { error, stateful })) => {
                log::error!("Control channel handler terminated with error: {}", error);
                errors.push(error);
                stateful
            }
            Err(join_error) => {
                panic!(
                    "Control channel handler task panicked leaving CC unrecoverable: {}",
                    join_error
                );
            }
        }
        .into_control_channel_server();

        let ec_server = match ec_handler_terminated {
            Ok(Ok(ec)) => ec,
            Ok(Err(Recovered { error, stateful })) => {
                log::error!("Event channel handler terminated with error: {}", error);
                errors.push(error);
                stateful
            }
            Err(join_error) => {
                panic!(
                    "Event channel handler task panicked leaving EC unrecoverable: {}",
                    join_error
                );
            }
        }
        .into_event_channel_server();

        let stateful = Server {
            state: SessionComplete {
                backend,
                cc_server,
                ec_server,
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

impl<B: Backend + 'static> Transitionable<SessionComplete<B>> for Server<Running<B>> {
    type NextStateful = Server<SessionComplete<B>>;

    fn transition(self) -> Self::NextStateful {
        self.stop();

        let backend = transition!(self.state.backend_task, backend::Terminated<B>).into_backend();
        let cc_server = transition!(self.state.cc_handler_task, cc_handler::Terminated)
            .into_control_channel_server();
        let ec_server = transition!(self.state.ec_handler_task, ec_handler::Terminated)
            .into_event_channel_server();
        Server {
            state: SessionComplete {
                backend,
                cc_server,
                ec_server,
            },
            config: self.config,
        }
    }
}

impl<B: Backend + 'static> Transitionable<ChannelsListening<B>> for Server<SessionComplete<B>> {
    type NextStateful = Server<ChannelsListening<B>>;

    fn transition(self) -> Self::NextStateful {
        Server {
            state: ChannelsListening {
                backend: self.state.backend,
                cc_server: self.state.cc_server,
                ec_server: self.state.ec_server,
            },
            config: self.config,
        }
    }
}

#[cfg(test)]
mod test {
    use crate::backend::X;
    use crate::server::*;

    use aperturec_channel::reliable::tcp;
    use aperturec_protocol::control::client_to_server as cm_c2s;
    use aperturec_protocol::control::server_to_client as cm_s2c;
    use aperturec_protocol::control::*;
    use aperturec_protocol::media::MediaKeepalive;
    use serial_test::serial;

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
            .root_program("glxgears")
            .build()
            .expect("ClientInit build")
            .into()
    }

    async fn server_cc_accepted(
        client_id: u64,
        cc_port: u16,
        ec_port: u16,
        mc_port: u16,
    ) -> (Server<ControlChannelAccepted<X>>, AsyncClientControlChannel) {
        let server_config = ConfigurationBuilder::default()
            .control_channel_addr(SocketAddr::new("127.0.0.1".parse().unwrap(), cc_port))
            .event_channel_addr(SocketAddr::new("127.0.0.1".parse().unwrap(), ec_port))
            .media_channel_addr(SocketAddr::new("127.0.0.1".parse().unwrap(), mc_port))
            .name("test server".into())
            .temp_client_id(client_id)
            .max_width(1920)
            .max_height(1080)
            .build()
            .expect("Configuration build");
        let server: Server<ChannelsListening<X>> = Server::new(server_config.clone())
            .expect("Create server instance")
            .try_transition()
            .await
            .expect("Failed initialize X backend")
            .try_transition()
            .await
            .expect("Failed to listen");
        let client_cc = client_cc(cc_port).await;
        let server: Server<ControlChannelAccepted<X>> = server
            .try_transition()
            .await
            .expect("Failed to accept CC client");
        (server, client_cc)
    }

    async fn server_client_authenticated(
        client_id: u64,
        cc_port: u16,
        ec_port: u16,
        mc_port: u16,
    ) -> (Server<AuthenticatedClient<X>>, AsyncClientControlChannel) {
        let (server, mut client_cc) =
            server_cc_accepted(client_id, cc_port, ec_port, mc_port).await;
        client_cc
            .send(client_init_msg(client_id))
            .await
            .expect("send ClientInit");
        let server = server.try_transition().await.expect("failed to auth");
        (server, client_cc)
    }

    async fn tcp_client(port: u16) -> tcp::Client<tcp::AsyncConnected> {
        tcp::Client::new(([127, 0, 0, 1], port))
            .try_transition()
            .await
            .expect("failed to connect")
            .try_transition()
            .await
            .expect("failed to async-ify")
    }

    async fn udp_client(port: u16) -> udp::Client<udp::AsyncConnected> {
        udp::Client::new(
            ([127, 0, 0, 1], port),
            SocketAddr::from(([127, 0, 0, 1], 0)),
        )
        .try_transition()
        .await
        .expect("failed to connect")
        .try_transition()
        .await
        .expect("failed to async-ify")
    }

    async fn client_cc(port: u16) -> AsyncClientControlChannel {
        AsyncClientControlChannel::new(tcp_client(port).await)
    }

    async fn client_ec(port: u16) -> AsyncClientEventChannel {
        AsyncClientEventChannel::new(tcp_client(port).await)
    }

    async fn client_mc(port: u16) -> AsyncClientMediaChannel {
        AsyncClientMediaChannel::new(udp_client(port).await)
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn auth_fail() {
        let (server, mut cc) = server_cc_accepted(1234, 8000, 8001, 8008).await;
        cc.send(client_init_msg(5678))
            .await
            .expect("send ClientInit");
        let _server_authed = server.try_transition().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn auth_pass() {
        let _ = server_client_authenticated(1234, 8002, 8003, 8008).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn client_init() {
        let (server, mut client_cc) = server_client_authenticated(1234, 8004, 8005, 8008).await;
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
        let _client_ec = client_ec(8005).await;

        for i in 0..server_init.decoder_areas.len() {
            let mut client_decoder = client_mc((8008 + i) as u16).await;
            let _ = client_decoder
                .send(mm_c2s::Message::MediaKeepalive(MediaKeepalive::new(
                    Decoder::new((1337 + i) as u32).into(),
                )))
                .await
                .expect("Send MediaKeepalive");
        }

        let _running: Server<Running<_>> = server
            .try_transition()
            .await
            .expect("failed to accept EC")
            .try_transition()
            .await
            .expect("failed to start running");
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
