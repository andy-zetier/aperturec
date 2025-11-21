use crate::{DisplayMode, gtk3};

use aperturec_graphics::prelude::*;
use aperturec_utils as utils;

use clap::Parser;
use std::num::{NonZeroUsize, ParseIntError};
use std::path::PathBuf;
use std::str::FromStr;
use std::thread;

/// Port forwarding argument specifying bind and forward addresses.
///
/// Format: `[bind_address:]bind_port:forward_address:forward_port`
#[derive(Clone, Debug, PartialEq)]
pub struct PortForwardArg {
    /// Optional bind address. If None, binds to all interfaces.
    pub bind_addr: Option<String>,
    /// Port to bind on the local side.
    pub bind_port: u16,
    /// Address to forward to on the remote side.
    pub forward_addr: String,
    /// Port to forward to on the remote side.
    pub forward_port: u16,
}

impl PortForwardArg {
    /// Converts port forward arguments into tunnel request descriptions.
    ///
    /// # Arguments
    ///
    /// * `client_bound_requests` - Local port forwards (bind on client, forward to server)
    /// * `server_bound_requests` - Remote port forwards (bind on server, forward to client)
    pub fn into_tunnel_requests<'a, I, J>(
        _client_bound_requests: I,
        _server_bound_requests: J,
    ) -> std::collections::BTreeMap<u64, aperturec_protocol::tunnel::Description>
    where
        I: IntoIterator<Item = &'a Self>,
        J: IntoIterator<Item = &'a Self>,
    {
        todo!()
    }
}

impl FromStr for PortForwardArg {
    type Err = PortForwardArgError;

    fn from_str(_s: &str) -> Result<PortForwardArg, PortForwardArgError> {
        todo!()
    }
}

/// Errors that can occur when parsing port forward arguments.
#[derive(Debug, thiserror::Error)]
pub enum PortForwardArgError {
    /// The bind port could not be parsed as a valid integer.
    #[error("Invalid bind port: {0}")]
    InvalidBindPort(ParseIntError),
    /// The forward port could not be parsed as a valid integer.
    #[error("Invalid forward port: {0}")]
    InvalidForwardPort(ParseIntError),
    /// The argument format is invalid.
    #[error(
        "Invalid format: expected '[bind_address:]bind_port:forward_address:forward_port', got '{0}'"
    )]
    InvalidFormat(String),
}

fn parse_resolution(s: &str) -> Result<Size, String> {
    let re = regex::Regex::new(r"^(\d+)[xX](\d+)$").expect("build regex");
    let Some((_, [width, height])) = re.captures(s).map(|c| c.extract()) else {
        return Err("Resolution must be in the form WIDTHxHEIGHT".to_string());
    };
    let width = width.parse().map_err(|w| format!("Invalid width: '{w}'"))?;
    let height = height
        .parse()
        .map_err(|h| format!("Invalid height: '{h}'"))?;
    Ok(Size::new(width, height))
}

#[derive(Debug, clap::Args)]
#[group(required = false, multiple = false)]
struct ResolutionGroup {
    /// Display size specified as WIDTHxHEIGHT
    #[arg(short, long, default_value = format!("{:?}", gtk3::DEFAULT_RESOLUTION), value_parser = parse_resolution)]
    resolution: Size,

    /// Set resolution to your displays' current sizes and startup in multi-monitor fullscreen
    /// mode. Fullscreen mode can be toggled at any time with Ctrl+Alt+Enter
    #[arg(short, long, action = clap::ArgAction::SetTrue)]
    fullscreen: bool,

    /// Set resolution to the current display's size and startup in single-monitor fullscreen mode.
    /// Single-monitor fullscreen mode can be toggled at any time with Ctrl+Alt+Shift+Enter
    #[arg(long, action = clap::ArgAction::SetTrue)]
    single_fullscreen: bool,
}

impl From<ResolutionGroup> for DisplayMode {
    fn from(g: ResolutionGroup) -> Self {
        if g.fullscreen {
            DisplayMode::MultiFullscreen
        } else if g.single_fullscreen {
            DisplayMode::SingleFullscreen
        } else {
            DisplayMode::Windowed { size: g.resolution }
        }
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about)]
pub struct Args {
    #[clap(flatten)]
    resolution: ResolutionGroup,

    /// Maximum number of decoders to use
    #[arg(short, long, default_value_t = thread::available_parallelism().unwrap())]
    decoder_max: NonZeroUsize,

    /// Program to launch and display on connection.
    ///
    /// If left unspecified, the client will open the server-specified root program, resuming any
    /// previous state. If specified, the client will force the server to launch a new instance of
    /// the specified program, which will be killed on client disconnect.
    ///
    /// Specifying this argument requires the server to have set `--allow-client-exec`.
    #[arg(index = 2)]
    program_cmdline: Option<String>,

    /// Hostname or IP address of the server, optionally including a control port. Eg. mybox.com,
    /// 10.10.10.10:46454, myotherbox.io:12345, [::1]
    #[arg(index = 1)]
    server_address: String,

    /// Additional TLS certificates to enable connections to servers that are not serving
    /// certificates signed by already installed CAs
    #[arg(short, long, default_values_os_t = Vec::<PathBuf>::new())]
    additional_tls_certificates: Vec<PathBuf>,

    /// Disable server certificate validation. Similar to `curl -k / --insecure`
    #[arg(short = 'k', long, action)]
    insecure: bool,

    /// Specifies that connections to the given TCP port on the client side are to be forwarded to
    /// the given host and port on the server side. Arguments are given in the form
    /// `[bind_address:]bind_port:forward_address:forward_port`
    ///
    /// If `bind_address` is left unspecified, the socket will bind to 0.0.0.0.
    #[arg(short = 'L', long)]
    local: Vec<PortForwardArg>,

    /// Specifies that connections to the given TCP port on the server side are to be forwarded to
    /// the given host and port on the client side. Arguments are given in the form
    /// `[bind_address:]bind_port:forward_address:forward_port`
    ///
    /// If `bind_address` is left unspecified, the socket will bind to 0.0.0.0. If `bind_port` is
    /// set to 0, the server will dynamically allocate a bind port and log it.
    #[arg(short = 'R', long)]
    remote: Vec<PortForwardArg>,

    #[clap(flatten)]
    log: utils::args::log::LogArgGroup,

    #[clap(flatten)]
    auth_token: utils::args::auth_token::AuthTokenAllArgGroup,

    #[clap(flatten)]
    metrics: utils::args::metrics::MetricsArgGroup,
}

/// Errors that can occur when parsing URI arguments.
#[derive(Debug, thiserror::Error)]
pub enum UriError {
    /// The URI scheme is invalid (must be "aperturec").
    #[error("Invalid URI scheme")]
    InvalidScheme,
    /// The URI is malformed.
    #[error("Malformed URI: {0}")]
    Malformed(String),
}

/// Errors that can occur when parsing command-line arguments.
#[derive(Debug, thiserror::Error)]
pub enum ArgsError {
    /// URI parsing failed.
    #[error(transparent)]
    Uri(#[from] UriError),
    /// Clap argument parsing failed.
    #[error("Argument parsing failed: {0}")]
    Parse(String),
}
