use crate::DisplayMode;

use aperturec_graphics::prelude::*;
use aperturec_protocol::tunnel;
use aperturec_utils as utils;

use clap::Parser;
use std::collections::BTreeMap;
use std::iter;
use std::num::{NonZeroUsize, ParseIntError};
use std::path::PathBuf;
use std::str::FromStr;
use std::thread;
use url::Url;

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
        client_bound_requests: I,
        server_bound_requests: J,
    ) -> BTreeMap<u64, aperturec_protocol::tunnel::Description>
    where
        I: IntoIterator<Item = &'a Self>,
        J: IntoIterator<Item = &'a Self>,
    {
        let arg2desc = |pf_arg: &PortForwardArg, side: tunnel::Side| tunnel::Description {
            side: side.into(),
            protocol: tunnel::Protocol::Tcp.into(),
            bind_address: pf_arg.bind_addr.clone().unwrap_or_default(),
            bind_port: pf_arg.bind_port as u32,
            forward_address: pf_arg.forward_addr.clone(),
            forward_port: pf_arg.forward_port as u32,
        };
        let client_bound_requests = client_bound_requests
            .into_iter()
            .map(|pf_arg| arg2desc(pf_arg, tunnel::Side::Client));
        let server_bound_requests = server_bound_requests
            .into_iter()
            .map(|pf_arg| arg2desc(pf_arg, tunnel::Side::Server));
        client_bound_requests
            .chain(server_bound_requests)
            .enumerate()
            .map(|(idx, desc)| (idx as u64, desc))
            .collect()
    }
}

/// Errors that occur when parsing port forwarding arguments.
#[derive(Debug, thiserror::Error)]
pub enum PortForwardArgError {
    #[error("Invalid bind port: {0}")]
    InvalidBindPort(ParseIntError),
    #[error("Invalid forward port: {0}")]
    InvalidForwardPort(ParseIntError),
    #[error("Invalid format: expected '[bind_address:]port:host:hostport', got '{0}'")]
    InvalidFormat(String),
}

impl FromStr for PortForwardArg {
    type Err = PortForwardArgError;

    fn from_str(s: &str) -> Result<PortForwardArg, PortForwardArgError> {
        // Split the input into parts by colon
        let parts: Vec<&str> = s.split(':').collect();

        match parts.len() {
            // Format: [bind_address:]bind_port:forward_addr:forward_port
            3 | 4 => {
                let bind_addr = if parts.len() == 4 {
                    Some(parts[0].to_string())
                } else {
                    None
                };
                let bind_port = parts[parts.len() - 3]
                    .parse::<u16>()
                    .map_err(PortForwardArgError::InvalidBindPort)?;
                let forward_addr = parts[parts.len() - 2].to_string();
                let forward_port = parts[parts.len() - 1]
                    .parse::<u16>()
                    .map_err(PortForwardArgError::InvalidForwardPort)?;

                Ok(PortForwardArg {
                    bind_addr,
                    bind_port,
                    forward_addr,
                    forward_port,
                })
            }
            _ => Err(PortForwardArgError::InvalidFormat(s.to_string())),
        }
    }
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

/// Mutually exclusive display mode arguments (multi-monitor fullscreen supported).
#[derive(Debug, clap::Args)]
#[group(required = false, multiple = false)]
pub struct ResolutionGroupMulti {
    /// Display size specified as WIDTHxHEIGHT
    #[arg(short, long, default_value = format!("{:?}", crate::DEFAULT_RESOLUTION), value_parser = parse_resolution)]
    resolution: Size,

    /// Set resolution to your displays' current sizes and start in multi-monitor fullscreen mode.
    #[arg(short, long, action = clap::ArgAction::SetTrue)]
    fullscreen: bool,

    /// Set resolution to the current display's size and start in single-monitor fullscreen mode.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    single_fullscreen: bool,
}

impl From<ResolutionGroupMulti> for DisplayMode {
    fn from(g: ResolutionGroupMulti) -> Self {
        if g.fullscreen {
            DisplayMode::MultiFullscreen
        } else if g.single_fullscreen {
            DisplayMode::SingleFullscreen
        } else {
            DisplayMode::Windowed { size: g.resolution }
        }
    }
}

/// Mutually exclusive display mode arguments (single-monitor fullscreen only).
#[derive(Debug, clap::Args)]
#[group(required = false, multiple = false)]
pub struct ResolutionGroupSingle {
    /// Display size specified as WIDTHxHEIGHT
    #[arg(short, long, default_value = format!("{:?}", crate::DEFAULT_RESOLUTION), value_parser = parse_resolution)]
    resolution: Size,

    /// Set resolution to the current display's size and start in single-monitor fullscreen mode.
    #[arg(short, long, visible_alias = "single-fullscreen", action = clap::ArgAction::SetTrue)]
    fullscreen: bool,
}

impl From<ResolutionGroupSingle> for DisplayMode {
    fn from(g: ResolutionGroupSingle) -> Self {
        if g.fullscreen {
            DisplayMode::SingleFullscreen
        } else {
            DisplayMode::Windowed { size: g.resolution }
        }
    }
}

/// Resolution argument groups supported by the client.
pub trait ResolutionGroup: clap::Args + Into<DisplayMode> {}

impl ResolutionGroup for ResolutionGroupMulti {}
impl ResolutionGroup for ResolutionGroupSingle {}

/// Command-line arguments for the ApertureC client.
#[derive(Parser, Debug)]
#[command(author, version, about)]
pub struct Args<R: ResolutionGroup = ResolutionGroupMulti> {
    #[clap(flatten)]
    pub resolution: R,

    /// Maximum number of decoders to use
    #[arg(short, long, default_value_t = thread::available_parallelism().unwrap())]
    pub decoder_max: NonZeroUsize,

    /// Program to launch and display on connection.
    ///
    /// If left unspecified, the client will open the server-specified root program, resuming any
    /// previous state. If specified, the client will force the server to launch a new instance of
    /// the specified program, which will be killed on client disconnect.
    ///
    /// Specifying this argument requires the server to have set `--allow-client-exec`.
    #[arg(index = 2)]
    pub program_cmdline: Option<String>,

    /// Hostname or IP address of the server, optionally including a control port. Eg. mybox.com,
    /// 10.10.10.10:46454, myotherbox.io:12345, [::1]
    #[arg(index = 1)]
    pub server_address: String,

    /// Additional TLS certificates to enable connections to servers that are not serving
    /// certificates signed by already installed CAs
    #[arg(short, long, default_values_os_t = Vec::<PathBuf>::new())]
    pub additional_tls_certificates: Vec<PathBuf>,

    /// Disable server certificate validation. Similar to `curl -k / --insecure`
    #[arg(short = 'k', long, action)]
    pub insecure: bool,

    /// Specifies that connections to the given TCP port on the client side are to be forwarded to
    /// the given host and port on the server side. Arguments are given in the form
    /// `[bind_address:]bind_port:forward_address:forward_port`
    ///
    /// If `bind_address` is left unspecified, the socket will bind to 0.0.0.0.
    #[arg(short = 'L', long)]
    pub local: Vec<PortForwardArg>,

    /// Specifies that connections to the given TCP port on the server side are to be forwarded to
    /// the given host and port on the client side. Arguments are given in the form
    /// `[bind_address:]bind_port:forward_address:forward_port`
    ///
    /// If `bind_address` is left unspecified, the socket will bind to 0.0.0.0. If `bind_port` is
    /// set to 0, the server will dynamically allocate a bind port and log it.
    #[arg(short = 'R', long)]
    pub remote: Vec<PortForwardArg>,

    #[clap(flatten)]
    pub log: utils::args::log::LogArgGroup,

    #[clap(flatten)]
    pub auth_token: utils::args::auth_token::AuthTokenAllArgGroup,
}

/// Errors that can occur when parsing command-line arguments.
#[derive(Debug, thiserror::Error)]
pub enum ArgsError {
    /// URI parsing failed.
    #[error(transparent)]
    Uri(#[from] UriError),
    /// Clap argument parsing failed.
    #[error("Argument parsing failed: {0}")]
    ClapError(#[from] clap::error::Error),
}

const URI_SCHEME: &str = "aperturec";

/// Errors that occur when parsing ApertureC URIs.
#[derive(Debug, thiserror::Error)]
pub enum UriError {
    #[error(transparent)]
    UrlParse(#[from] url::ParseError),
    #[error("URI scheme should be '{URI_SCHEME}', is '{0}'")]
    BadScheme(String),
    #[error("URI provides a username")]
    ProvidesUsername,
    #[error("URI provides a fragment")]
    ProvidesFragment,
    #[error("URI has no host")]
    NoHost,
}

impl<R: ResolutionGroup> Args<R> {
    /// Parses arguments from an ApertureC URI.
    ///
    /// URIs follow the format `aperturec://host[:port][?param=value&...]`.
    /// Query parameters are mapped to command-line flags.
    pub fn from_uri(uri: &str) -> Result<Args<R>, ArgsError> {
        let parsed_uri = Url::parse(uri).map_err(UriError::UrlParse)?;
        if parsed_uri.scheme() != URI_SCHEME {
            return Err(UriError::BadScheme(parsed_uri.scheme().to_string()).into());
        }
        if !parsed_uri.username().is_empty() {
            return Err(UriError::ProvidesUsername.into());
        }
        if parsed_uri.fragment().is_some() {
            return Err(UriError::ProvidesFragment.into());
        }

        let mut host = parsed_uri.host_str().ok_or(UriError::NoHost)?.to_string();
        if let Some(port) = parsed_uri.port() {
            host = format!("{host}:{port}");
        }
        let args = parsed_uri.query_pairs().flat_map(|(k, v)| {
            let k = if k.len() == 1 {
                format!("-{k}")
            } else {
                format!("--{k}")
            };

            if v.is_empty() {
                vec![k]
            } else {
                vec![k, v.to_string()]
            }
        });
        Ok(Args::<R>::try_parse_from(
            iter::once(env!("CARGO_PKG_NAME").to_string())
                .chain(iter::once(host))
                .chain(args),
        )?)
    }
}
