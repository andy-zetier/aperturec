use crate::backend::Backend;
use crate::task::{
    backend, control_channel_handler as cc_handler, encoder, event_channel_handler as ec_handler,
    heartbeat,
};

use anyhow::{anyhow, Result};
use aperturec_channel::reliable::tcp;
use aperturec_channel::*;
use aperturec_protocol::common_types::*;
use aperturec_protocol::control_messages as cm;
use aperturec_protocol::*;
use aperturec_state_machine::*;
use async_trait::async_trait;
use derive_builder::Builder;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[derive(Builder, Debug, Clone)]
pub struct Configuration {
    control_channel_addr: SocketAddr,
    event_channel_addr: SocketAddr,
    name: String,
    temp_client_id: ClientId,
    initial_program: String,
    max_width: usize,
    max_height: usize,
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
    decoder_areas: Vec<cm::DecoderArea>,
    codecs: BTreeMap<u16, Codec>,
}

#[derive(State)]
pub struct ChannelsAccepted<B: Backend + 'static> {
    backend: B,
    cc: AsyncServerControlChannel,
    ec: AsyncServerEventChannel,
    client_hb_interval: Duration,
    client_hb_response_interval: Duration,
    decoder_areas: Vec<cm::DecoderArea>,
    codecs: BTreeMap<u16, Codec>,
}

#[derive(State)]
pub struct Running<B: Backend + 'static> {
    backend_task: backend::Task<backend::Running<B>>,
    backend_ct: CancellationToken,
    heartbeat_task: heartbeat::Task<heartbeat::Running>,
    heartbeat_ct: CancellationToken,
    cc_handler_task: cc_handler::Task<cc_handler::Running>,
    cc_handler_ct: CancellationToken,
    ec_handler_task: ec_handler::Task<ec_handler::Running>,
    ec_handler_ct: CancellationToken,
    received_goodbye: oneshot::Receiver<()>,
}

#[derive(State)]
pub struct SessionComplete<B: Backend + 'static> {
    backend: B,
    cc_server: tcp::Server<tcp::Listening>,
    ec_server: tcp::Server<tcp::Listening>,
}

impl Server<Created> {
    pub fn new(config: Configuration) -> Self {
        Server {
            state: Created,
            config,
        }
    }
}

impl<B: Backend + 'static> Server<Running<B>> {
    pub fn stop(&self) {
        self.state.backend_ct.cancel();
        self.state.heartbeat_ct.cancel();
        self.state.cc_handler_ct.cancel();
        self.state.ec_handler_ct.cancel();
    }
}

fn do_partition(num_partitions: usize, rect: encoder::Rect) -> Vec<encoder::Rect> {
    if num_partitions == 0 {
        return vec![];
    } else if num_partitions == 1 {
        return vec![rect];
    }

    let num_partitions_per_half = num_partitions / 2;
    let (lhs_rect, rhs_rect) = if rect.size.width > rect.size.height {
        let child_width = rect.size.width / 2;
        let child_size = encoder::Size::new(child_width, rect.size.height);

        (
            encoder::Rect {
                size: child_size,
                origin: rect.origin,
            },
            encoder::Rect {
                size: child_size,
                origin: rect.origin + encoder::Size::new(child_width, 0),
            },
        )
    } else {
        let child_height = rect.size.height / 2;
        let child_size = encoder::Size::new(rect.size.width, child_height);

        (
            encoder::Rect {
                size: child_size,
                origin: rect.origin,
            },
            encoder::Rect {
                size: child_size,
                origin: rect.origin + encoder::Size::new(0, child_height),
            },
        )
    };
    let mut lhs = do_partition(num_partitions_per_half, lhs_rect);
    let mut rhs = do_partition(num_partitions_per_half, rhs_rect);
    lhs.append(&mut rhs);
    lhs
}

fn partition(
    client_resolution: &Dimension,
    decoders: &[cm::Decoder],
) -> (encoder::Size, Vec<cm::DecoderArea>) {
    let n_encoders = if decoders.len() > 1 {
        decoders.len() / 2 * 2
    } else {
        decoders.len()
    };
    let client_resolution = encoder::Size::new(
        client_resolution.width as usize,
        client_resolution.height as usize,
    );
    let partitions = do_partition(
        n_encoders,
        encoder::Rect {
            size: client_resolution,
            origin: encoder::Point::new(0, 0),
        },
    );

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
    (
        server_resolution,
        partitions
            .iter()
            .zip(decoders)
            .map(|(rect, decoder)| {
                cm::DecoderArea::new(
                    decoder.clone(),
                    Location::new(rect.origin.x as u64, rect.origin.y as u64),
                    Dimension::new(rect.size.width as u64, rect.size.height as u64),
                )
            })
            .collect(),
    )
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
                    B::initialize(
                        &self.config.initial_program,
                        self.config.max_width,
                        self.config.max_height
                    )
                    .await,
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
            control_messages::ClientToServerMessage::ClientInit(client_init) => client_init,
            _ => return_recover!(self, "non client init message received"),
        };
        log::trace!("Client init: {:#?}", client_init);
        if client_init.temp_id != self.config.temp_client_id {
            return_recover!(self, "mismatched temporary ID");
        }
        if client_init.decoders.is_empty() {
            return_recover!(self, "client did not provide any decoders");
        }

        let client_resolution = &client_init.client_info.display_size;
        let (resolution, decoder_areas) = partition(client_resolution, &client_init.decoders);
        let codecs = decoder_areas
            .iter()
            .map(|decoder_area| {
                (
                    decoder_area.decoder.port,
                    if client_init
                        .client_caps
                        .supported_codecs
                        .contains(&Codec::new_zlib())
                    {
                        Codec::new_zlib()
                    } else {
                        Codec::new_raw()
                    },
                )
            })
            .collect();
        try_recover!(self.state.backend.set_resolution(&resolution).await, self);
        log::trace!("Resolution set to {:?}", resolution);
        let cursor_bitmaps = try_recover!(self.state.backend.cursor_bitmaps().await, self);

        let client_id = ClientId(rand::random());
        let server_init = try_recover!(
            cm::ServerInitBuilder::default()
                .client_id(client_id.clone())
                .server_name(self.config.name.clone())
                .cursor_bitmaps(cursor_bitmaps)
                .decoder_areas(decoder_areas.clone())
                .event_port(self.config.event_channel_addr.port())
                .display_size(Dimension::new(
                    resolution.width as u64,
                    resolution.height as u64
                ))
                .build(),
            self
        );
        log::trace!("Server init: {:#?}", server_init);

        try_recover!(
            self.state
                .cc
                .send(cm::ServerToClientMessage::new_server_init(server_init))
                .await,
            self
        );

        Ok(Server {
            state: AuthenticatedClient {
                backend: self.state.backend,
                cc: self.state.cc,
                ec_server: self.state.ec_server,
                client_hb_interval: Duration::from_millis(client_init.client_heartbeat_interval.0),
                client_hb_response_interval: Duration::from_millis(
                    client_init.client_heartbeat_response_interval.0,
                ),
                decoder_areas,
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

        Ok(Server {
            state: ChannelsAccepted {
                backend: self.state.backend,
                cc: self.state.cc,
                ec,
                client_hb_interval: self.state.client_hb_interval,
                client_hb_response_interval: self.state.client_hb_response_interval,
                decoder_areas: self.state.decoder_areas,
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
        let (cc_handler_task, cc_handler_channels, cc_handler_ct) =
            cc_handler::Task::new(self.state.cc);
        let (ec_handler_task, ec_handler_channels, ec_handler_ct) =
            ec_handler::Task::new(self.state.ec);

        let (backend_task, backend_channels, backend_ct) =
            backend::Task::<backend::Created<B>>::new(
                self.state.backend,
                &self.state.decoder_areas,
                self.state.codecs,
                &remote_addr,
                ec_handler_channels.event_rx,
                cc_handler_channels.fb_update_req_rx,
                cc_handler_channels.missed_frame_rx,
            );
        let (heartbeat_task, heartbeat_ct) = heartbeat::Task::<heartbeat::Created>::new(
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
                backend_task,
                backend_ct,
                heartbeat_task,
                heartbeat_ct,
                cc_handler_task,
                cc_handler_ct,
                ec_handler_task,
                ec_handler_ct,
                received_goodbye: cc_handler_channels.received_goodbye,
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

        let mut backend_task =
            tokio::spawn(async move { self.state.backend_task.try_transition().await });
        let mut heartbeat_task =
            tokio::spawn(async move { self.state.heartbeat_task.try_transition().await });
        let mut cc_handler_task =
            tokio::spawn(async move { self.state.cc_handler_task.try_transition().await });
        let mut ec_handler_task =
            tokio::spawn(async move { self.state.ec_handler_task.try_transition().await });

        let mut has_cancelled_subtasks = false;
        loop {
            tokio::select! {
                _ = &mut self.state.received_goodbye, if !has_cancelled_subtasks => {
                    self.state.backend_ct.cancel();
                    self.state.cc_handler_ct.cancel();
                    self.state.ec_handler_ct.cancel();
                    self.state.heartbeat_ct.cancel();
                    has_cancelled_subtasks = true;
                }
                be_term = &mut backend_task, if backend_terminated.is_none() => {
                    backend_terminated = Some(be_term);
                    self.state.heartbeat_ct.cancel();
                    self.state.cc_handler_ct.cancel();
                    self.state.ec_handler_ct.cancel();
                    has_cancelled_subtasks = true;
                }
                hb_term = &mut heartbeat_task, if heartbeat_terminated.is_none() => {
                    heartbeat_terminated = Some(hb_term);
                    self.state.backend_ct.cancel();
                    self.state.cc_handler_ct.cancel();
                    self.state.ec_handler_ct.cancel();
                    has_cancelled_subtasks = true;
                }
                cc_term = &mut cc_handler_task, if cc_handler_terminated.is_none() => {
                    cc_handler_terminated = Some(cc_term);
                    self.state.backend_ct.cancel();
                    self.state.heartbeat_ct.cancel();
                    self.state.ec_handler_ct.cancel();
                    has_cancelled_subtasks = true;
                }
                ec_term = &mut ec_handler_task, if ec_handler_terminated.is_none() => {
                    ec_handler_terminated = Some(ec_term);
                    self.state.backend_ct.cancel();
                    self.state.heartbeat_ct.cancel();
                    self.state.cc_handler_ct.cancel();
                    has_cancelled_subtasks = true;
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
    use aperturec_protocol::control_messages::*;
    use serial_test::serial;

    fn client_init_msg(id: u64) -> ClientToServerMessage {
        ClientToServerMessage::new_client_init(
            ClientInitBuilder::default()
                .temp_id(ClientId(id))
                .client_info(
                    ClientInfoBuilder::default()
                        .version(
                            SemVerBuilder::default()
                                .major(0)
                                .minor(1)
                                .patch(2)
                                .build()
                                .expect("SemVer build"),
                        )
                        .build_id("asdf".into())
                        .os(Os::new_linux())
                        .os_version("Bionic Beaver".into())
                        .ssl_library("OpenSSL".into())
                        .ssl_version("1.2".into())
                        .bitness(Bitness::new_b64())
                        .endianness(Endianness::new_big())
                        .architecture(Architecture::new_x86())
                        .cpu_id("Haswell".into())
                        .number_of_cores(4)
                        .amount_of_ram("2.4Gb".into())
                        .display_size(
                            DimensionBuilder::default()
                                .width(1024)
                                .height(768)
                                .build()
                                .expect("Dimension build"),
                        )
                        .build()
                        .expect("ClientInfo build"),
                )
                .client_caps(
                    ClientCapsBuilder::default()
                        .supported_codecs(vec![Codec::new_zlib()])
                        .build()
                        .expect("ClientCaps build"),
                )
                .client_heartbeat_interval(DurationMs(1000))
                .client_heartbeat_response_interval(DurationMs(1000))
                .decoders(vec![
                    Decoder::new(9990),
                    Decoder::new(9991),
                    Decoder::new(9992),
                ])
                .build()
                .expect("ClientInit build"),
        )
    }

    async fn server_cc_accepted(
        client_id: u64,
        cc_port: u16,
        ec_port: u16,
    ) -> (Server<ControlChannelAccepted<X>>, AsyncClientControlChannel) {
        let server_config = ConfigurationBuilder::default()
            .control_channel_addr(SocketAddr::new("127.0.0.1".parse().unwrap(), cc_port))
            .event_channel_addr(SocketAddr::new("127.0.0.1".parse().unwrap(), ec_port))
            .name("test server".into())
            .temp_client_id(ClientId(client_id))
            .initial_program("glxgears".to_owned())
            .max_width(1920)
            .max_height(1080)
            .build()
            .expect("Configuration build");
        let server: Server<ChannelsListening<X>> = Server::new(server_config.clone())
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
    ) -> (Server<AuthenticatedClient<X>>, AsyncClientControlChannel) {
        let (server, mut client_cc) = server_cc_accepted(client_id, cc_port, ec_port).await;
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

    async fn client_cc(port: u16) -> AsyncClientControlChannel {
        AsyncClientControlChannel::new(tcp_client(port).await)
    }

    async fn client_ec(port: u16) -> AsyncClientEventChannel {
        AsyncClientEventChannel::new(tcp_client(port).await)
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn auth_fail() {
        let (server, mut cc) = server_cc_accepted(1234, 8000, 8001).await;
        cc.send(client_init_msg(5678))
            .await
            .expect("send ClientInit");
        let _server_authed = server.try_transition().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn auth_pass() {
        let _ = server_client_authenticated(1234, 8002, 8003).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn client_init() {
        let (server, mut client_cc) = server_client_authenticated(1234, 8004, 8005).await;
        let msg = client_cc.receive().await.expect("receiving server init");
        let server_init = if let cm::ServerToClientMessage::ServerInit(server_init) = msg {
            assert_eq!(server_init.decoder_areas.len(), 2);
            server_init
        } else {
            panic!("non server init message");
        };

        assert_eq!(server_init.decoder_areas.len(), 2);
        assert_eq!(server_init.decoder_areas[0].dimension.width, 512);
        assert_eq!(server_init.decoder_areas[1].dimension.width, 512);
        assert_eq!(server_init.decoder_areas[0].dimension.height, 768);
        assert_eq!(server_init.decoder_areas[1].dimension.height, 768);
        assert_eq!(server_init.decoder_areas[0].location.x_position, 0);
        assert_eq!(server_init.decoder_areas[1].location.x_position, 512);
        assert_eq!(server_init.decoder_areas[0].location.y_position, 0);
        assert_eq!(server_init.decoder_areas[1].location.y_position, 0);
        assert_eq!(server_init.display_size.width, 1024);
        assert_eq!(server_init.display_size.height, 768);
        let _client_ec = client_ec(8005).await;
        let _running: Server<Running<_>> = server
            .try_transition()
            .await
            .expect("failed to accept EC")
            .try_transition()
            .await
            .expect("failed to start running");
    }
}
