pub mod codec;
pub mod reliable;

use aperturec_protocol::*;

pub type ServerControlChannel = codec::der::reliable::Duplex<
    reliable::tcp::Server<reliable::tcp::Accepted>,
    control_messages::ClientToServerMessage,
    control_messages::ServerToClientMessage,
>;

pub type ClientControlChannel = codec::der::reliable::Duplex<
    reliable::tcp::Client<reliable::tcp::Connected>,
    control_messages::ServerToClientMessage,
    control_messages::ClientToServerMessage,
>;

pub type ServerEventChannel = codec::der::reliable::ReceiverSimplex<
    reliable::tcp::Server<reliable::tcp::Accepted>,
    event_messages::ClientToServerMessage,
>;

pub type ClientEventChannel = codec::der::reliable::SenderSimplex<
    reliable::tcp::Client<reliable::tcp::Connected>,
    event_messages::ClientToServerMessage,
>;

pub type ServerMediaChannel = codec::der::reliable::SenderSimplex<
    reliable::tcp::Server<reliable::tcp::Accepted>,
    media_messages::ServerToClientMessage,
>;

pub type ClientMediaChannel = codec::der::reliable::ReceiverSimplex<
    reliable::tcp::Client<reliable::tcp::Connected>,
    media_messages::ServerToClientMessage,
>;

pub type AsyncServerControlChannel = codec::der::reliable::Duplex<
    reliable::tcp::Server<reliable::tcp::AsyncAccepted>,
    control_messages::ClientToServerMessage,
    control_messages::ServerToClientMessage,
>;

pub type AsyncClientControlChannel = codec::der::reliable::Duplex<
    reliable::tcp::Client<reliable::tcp::AsyncConnected>,
    control_messages::ServerToClientMessage,
    control_messages::ClientToServerMessage,
>;

pub type AsyncServerEventChannel = codec::der::reliable::ReceiverSimplex<
    reliable::tcp::Server<reliable::tcp::AsyncAccepted>,
    event_messages::ClientToServerMessage,
>;

pub type AsyncClientEventChannel = codec::der::reliable::SenderSimplex<
    reliable::tcp::Client<reliable::tcp::AsyncConnected>,
    event_messages::ClientToServerMessage,
>;

pub type AsyncServerMediaChannel = codec::der::reliable::SenderSimplex<
    reliable::tcp::Server<reliable::tcp::AsyncAccepted>,
    media_messages::ServerToClientMessage,
>;

pub type AsyncClientMediaChannel = codec::der::reliable::ReceiverSimplex<
    reliable::tcp::Client<reliable::tcp::AsyncConnected>,
    media_messages::ServerToClientMessage,
>;
