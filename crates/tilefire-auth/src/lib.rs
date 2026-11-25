use std::ffi::{OsStr, OsString};

use nonstick::{
    AuthnFlags, ConversationAdapter, ErrorCode, Result as PamResult,
    Transaction, TransactionBuilder,
};
use thiserror::Error;
use zeroize::Zeroizing;

pub struct Password(Zeroizing<String>);

impl Password {
    pub fn new(password: impl Into<String>) -> Self {
        Self(Zeroizing::new(password.into()))
    }
}

impl std::fmt::Debug for Password {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Password([redacted])")
    }
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("PAM authentication failed: {0}")]
    Pam(#[from] ErrorCode),
}

#[derive(Clone, Debug)]
pub struct Authenticator {
    service: String,
}

impl Default for Authenticator {
    fn default() -> Self {
        Self::new("tilefire")
    }
}

impl Authenticator {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    pub fn from_env() -> Self {
        std::env::var("TILEFIRE_PAM_SERVICE")
            .map(Self::new)
            .unwrap_or_default()
    }

    pub fn authenticate(
        &self,
        username: &str,
        password: Password,
    ) -> Result<(), AuthError> {
        let conversation = LockConversation {
            username: username.to_owned(),
            password,
        };
        let mut transaction =
            TransactionBuilder::new_with_service(&self.service)
                .username(username)
                .build(conversation.into_conversation())?;
        transaction.authenticate(AuthnFlags::empty())?;
        transaction.account_management(AuthnFlags::empty())?;
        Ok(())
    }
}

struct LockConversation {
    username: String,
    password: Password,
}

impl ConversationAdapter for LockConversation {
    fn prompt(&self, _request: impl AsRef<OsStr>) -> PamResult<OsString> {
        Ok(OsString::from(&self.username))
    }

    fn masked_prompt(
        &self,
        _request: impl AsRef<OsStr>,
    ) -> PamResult<OsString> {
        Ok(OsString::from(self.password.0.as_str()))
    }

    fn error_msg(&self, _message: impl AsRef<OsStr>) {}

    fn info_msg(&self, _message: impl AsRef<OsStr>) {}
}
