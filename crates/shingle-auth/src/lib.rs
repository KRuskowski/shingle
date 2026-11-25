use std::ffi::{OsStr, OsString};

use nonstick::{
  AuthnFlags, ConversationAdapter, ErrorCode, Result as PamResult, Transaction,
  TransactionBuilder,
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
    Self::new(default_service(|path| path.exists()))
  }
}

/// Returns the PAM service name to use when none is configured.
///
/// PAM falls back to the restrictive `other` service when a service file is
/// missing, which would make unlocking impossible. A system that only has the
/// pre-rename `tilefire` service installed therefore keeps using it.
fn default_service(exists: impl Fn(&std::path::Path) -> bool) -> &'static str {
  let pam_dir = std::path::Path::new("/etc/pam.d");
  if !exists(&pam_dir.join("shingle")) && exists(&pam_dir.join("tilefire")) {
    "tilefire"
  } else {
    "shingle"
  }
}

impl Authenticator {
  pub fn new(service: impl Into<String>) -> Self {
    Self {
      service: service.into(),
    }
  }

  pub fn from_env() -> Self {
    // TILEFIRE_PAM_SERVICE is the pre-rename name and is still honoured.
    std::env::var("SHINGLE_PAM_SERVICE")
      .or_else(|_| std::env::var("TILEFIRE_PAM_SERVICE"))
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
    let mut transaction = TransactionBuilder::new_with_service(&self.service)
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

  fn masked_prompt(&self, _request: impl AsRef<OsStr>) -> PamResult<OsString> {
    Ok(OsString::from(self.password.0.as_str()))
  }

  fn error_msg(&self, _message: impl AsRef<OsStr>) {}

  fn info_msg(&self, _message: impl AsRef<OsStr>) {}
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn default_service_is_shingle() {
    assert_eq!(default_service(|_| true), "shingle");
    assert_eq!(default_service(|_| false), "shingle");
  }

  #[test]
  fn default_service_keeps_installed_legacy_tilefire_service() {
    let service = default_service(|path| path.ends_with("tilefire"));
    assert_eq!(service, "tilefire");
  }
}
