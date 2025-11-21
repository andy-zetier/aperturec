use crate::DisplayMode;
use crate::args::{Args, ArgsError, PortForwardArg};

use aperturec_utils as utils;

use derive_builder::Builder;
use openssl::x509::X509;
use secrecy::SecretString;
use std::io;
use std::num::NonZeroUsize;

/// Client configuration for connecting to an ApertureC server.
///
/// This structure contains all the necessary configuration to establish
/// and maintain a connection to an ApertureC server, including authentication,
/// display settings, and tunnel configurations.
#[derive(Builder, Clone, Debug)]
#[builder(setter(into))]
pub struct Configuration {
    /// Client name identifier.
    pub name: String,
    /// Authentication token for server access.
    pub auth_token: SecretString,
    /// Maximum number of decoder threads to use.
    pub decoder_max: NonZeroUsize,
    /// Server address to connect to.
    pub server_addr: String,
    /// Initial display mode for the client.
    pub initial_display_mode: DisplayMode,
    /// Optional command line to execute on the server.
    #[builder(setter(strip_option), default)]
    pub program_cmdline: Option<String>,
    /// Additional TLS certificates to trust.
    #[builder(setter(name = "additional_tls_certificate", custom), default)]
    pub additional_tls_certificates: Vec<X509>,
    /// Whether to allow insecure TLS connections.
    #[builder(default)]
    pub allow_insecure_connection: bool,
    /// Client-bound tunnel requests (local port forwards).
    #[builder(default)]
    pub client_bound_tunnel_reqs: Vec<PortForwardArg>,
    /// Server-bound tunnel requests (remote port forwards).
    #[builder(default)]
    pub server_bound_tunnel_reqs: Vec<PortForwardArg>,
}

impl Configuration {
    /// Creates configuration automatically from CLI arguments or URI.
    ///
    /// This method attempts to parse configuration from command-line arguments,
    /// or from an ApertureC URI if the first argument looks like a URI.
    pub fn auto() -> Result<Self, ConfigurationError> {
        todo!()
    }

    /// Creates configuration from parsed arguments.
    pub fn from_args(_args: Args) -> Result<Self, ConfigurationError> {
        todo!()
    }

    /// Creates configuration by parsing command-line arguments.
    pub fn from_argv() -> Result<Self, ConfigurationError> {
        todo!()
    }

    /// Creates configuration from an ApertureC URI.
    ///
    /// URIs follow the format: `aperturec://[auth_token@]server_address[?query_params]`
    pub fn from_uri(_uri: &str) -> Result<Self, ConfigurationError> {
        todo!()
    }
}

impl ConfigurationBuilder {
    /// Adds an additional TLS certificate to trust.
    ///
    /// This method can be called multiple times to add multiple certificates.
    pub fn additional_tls_certificate(&mut self, _additional_tls_cert: X509) -> &mut Self {
        todo!()
    }
}

/// Errors that can occur during configuration creation.
#[derive(Debug, thiserror::Error)]
pub enum ConfigurationError {
    /// Argument parsing failed.
    #[error(transparent)]
    Args(#[from] ArgsError),
    /// Failed to get authentication token from user prompt.
    #[error("failed to get auth token from prompt")]
    AuthTokenPrompt(#[source] io::Error),
    /// Authentication token argument parsing failed.
    #[error(transparent)]
    AuthTokenArg(#[from] utils::args::auth_token::AuthTokenError),
    /// Failed to parse hostname from server address.
    #[error("failed parsing hostname from '{0}'")]
    Hostname(String),
    /// Failed to read TLS certificate or key material.
    #[error("failed to read TLS material")]
    TlsMaterialRead(#[source] io::Error),
    /// Failed to parse X.509 certificate.
    #[error("failed to parse x509")]
    X509Parse(#[from] openssl::error::ErrorStack),
    /// Configuration builder failed to build the configuration.
    #[error("failed to build Configuration")]
    Build(#[from] ConfigurationBuilderError),
}
