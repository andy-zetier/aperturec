use secrecy::SecretString;
use std::path::PathBuf;
use std::{fs, io};

#[derive(Debug, thiserror::Error)]
pub enum AuthTokenError {
    #[error("Failed to read token from stdin: {0}")]
    ReadFromStdin(#[source] io::Error),
    #[error("Failed to read token from file {path}: {source}")]
    ReadFromFile { path: PathBuf, source: io::Error },
}

#[derive(Debug, clap::Args)]
pub struct AuthTokenFileArgGroup {
    /// File containing authentication token that a client can use to connect to the server. Pass
    /// "-" to read authentication token from stdin.
    #[arg(long)]
    auth_token_file: Option<PathBuf>,
}

impl AuthTokenFileArgGroup {
    pub fn into_token(self) -> Result<Option<SecretString>, AuthTokenError> {
        if let Some(path) = self.auth_token_file {
            let auth_token = if path == PathBuf::from("-") {
                SecretString::from(
                    io::read_to_string(io::stdin())
                        .map_err(AuthTokenError::ReadFromStdin)?
                        .trim(),
                )
            } else {
                SecretString::from(
                    fs::read_to_string(&path)
                        .map_err(|source| AuthTokenError::ReadFromFile { path, source })?
                        .trim(),
                )
            };
            Ok(Some(auth_token))
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct AuthTokenDirectArgGroup {
    /// Authentication token that a client can use to connect to the server.
    #[arg(short = 't', long, conflicts_with = "auth_token_file")]
    auth_token: Option<String>,
}

impl AuthTokenDirectArgGroup {
    pub fn into_token(self) -> Option<SecretString> {
        self.auth_token.map(SecretString::from)
    }
}

#[derive(Debug, clap::Args)]
pub struct AuthTokenAllArgGroup {
    #[clap(flatten)]
    file: AuthTokenFileArgGroup,

    #[clap(flatten)]
    direct: AuthTokenDirectArgGroup,
}

impl AuthTokenAllArgGroup {
    pub fn into_token(self) -> Result<Option<SecretString>, AuthTokenError> {
        Ok(self.file.into_token()?.or(self.direct.into_token()))
    }
}
