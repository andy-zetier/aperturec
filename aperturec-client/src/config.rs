use crate::{
    DisplayMode,
    args::{Args, ArgsError, PortForwardArg},
};

use aperturec_utils::{args, warn_early};

use clap::Parser;
use derive_builder::Builder;
use gethostname::gethostname;
use openssl::x509::X509;
use secrecy::SecretString;
use std::{env, ffi::OsString, fs, io, num::NonZeroUsize};

/// Client configuration for connecting to an ApertureC server.
#[derive(Builder, Clone, Debug)]
pub struct Configuration {
    /// Client hostname.
    pub name: String,
    /// Authentication token for the server.
    pub auth_token: SecretString,
    /// Maximum number of parallel decoders.
    pub decoder_max: NonZeroUsize,
    /// Server address (hostname or IP, optionally with port).
    pub server_addr: String,
    /// Initial display mode (windowed or fullscreen).
    pub initial_display_mode: DisplayMode,
    /// Optional program command line to execute on connect.
    #[builder(setter(strip_option), default)]
    pub program_cmdline: Option<String>,
    /// Additional TLS certificates for server verification.
    #[builder(setter(name = "additional_tls_certificate", custom), default)]
    pub additional_tls_certificates: Vec<X509>,
    /// Allow insecure TLS connections (skip certificate validation).
    #[builder(default)]
    pub allow_insecure_connection: bool,
    /// Client-bound port forwarding requests (`-L`).
    #[builder(default)]
    pub client_bound_tunnel_reqs: Vec<PortForwardArg>,
    /// Server-bound port forwarding requests (`-R`).
    #[builder(default)]
    pub server_bound_tunnel_reqs: Vec<PortForwardArg>,
}

impl Configuration {
    /// Creates configuration automatically from CLI arguments or URI.
    ///
    /// Checks for a single URI argument or the `AC_URI` environment variable,
    /// otherwise parses standard command-line arguments.
    pub fn auto() -> Result<Self, ConfigurationError> {
        let args = if env::args().count() == 2 {
            let arg1 = env::args().nth(1).unwrap();
            if let Ok(parsed) = Args::from_uri(&arg1) {
                parsed
            } else if let Ok(uri) = env::var("AC_URI") {
                warn_early!(
                    "CLI arguments are ignored when using AC_URI. Unset AC_URI if you would like to use CLI arguments."
                );
                Args::from_uri(&uri)?
            } else {
                Args::parse()
            }
        } else if let Ok(uri) = env::var("AC_URI") {
            if env::args().count() > 1 {
                warn_early!(
                    "CLI arguments are ignored when using AC_URI. Unset AC_URI if you would like to use CLI arguments."
                );
            }
            Args::from_uri(&uri)?
        } else {
            Args::parse()
        };

        Configuration::from_args(args)
    }

    /// Creates configuration from parsed arguments.
    ///
    /// Prompts for an authentication token if not provided in arguments.
    pub fn from_args(args: Args) -> Result<Self, ConfigurationError> {
        let mut config_builder = ConfigurationBuilder::default();
        let auth_token = match args.auth_token.into_token()? {
            Some(token) => token,
            None => SecretString::from(
                rpassword::prompt_password(format!(
                    "Authentication token for {}: ",
                    args.server_address
                ))
                .map_err(ConfigurationError::AuthTokenPrompt)?,
            ),
        };
        config_builder
            .decoder_max(args.decoder_max)
            .name(
                gethostname()
                    .into_string()
                    .map_err(ConfigurationError::Hostname)?,
            )
            .server_addr(args.server_address)
            .auth_token(auth_token)
            .initial_display_mode(args.resolution.into())
            .allow_insecure_connection(args.insecure)
            .client_bound_tunnel_reqs(args.local)
            .server_bound_tunnel_reqs(args.remote);
        if let Some(program_cmdline) = args.program_cmdline {
            config_builder.program_cmdline(program_cmdline);
        }
        for cert_path in args.additional_tls_certificates {
            config_builder.additional_tls_certificate(
                X509::from_pem(&fs::read(cert_path).map_err(ConfigurationError::TlsMaterialRead)?)
                    .map_err(ConfigurationError::X509Parse)?,
            );
        }
        Ok(config_builder.build()?)
    }

    /// Creates a new ApertureC client configuration by parsing command-line arguments.
    ///
    /// This function reads arguments from the process's command line (via `std::env::args`)
    /// and constructs a configuration object.
    pub fn from_argv() -> Result<Self, ConfigurationError> {
        Configuration::from_args(Args::try_parse().map_err(ArgsError::ClapError)?)
    }

    /// Creates configuration from an ApertureC URI.
    ///
    /// URIs follow the format: `aperturec://[auth_token@]server_address[?query_params]`
    pub fn from_uri(uri: &str) -> Result<Self, ConfigurationError> {
        Configuration::from_args(Args::from_uri(uri)?)
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
    AuthTokenArg(#[from] args::auth_token::AuthTokenError),
    /// Failed to parse hostname from server address.
    #[error("failed parsing hostname from '{0:?}'")]
    Hostname(OsString),
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
