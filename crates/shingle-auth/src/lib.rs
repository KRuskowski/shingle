//! PAM authentication for `shingle-lock`.
//!
//! [`Authenticator`] runs one PAM transaction per attempt: `pam_authenticate`
//! followed by `pam_acct_mgmt`, answering every PAM prompt from the username
//! and [`Password`] it was given. The call blocks for as long as the PAM
//! stack does, including any failure delay a module imposes, so the lock
//! client runs it on a worker thread (crates/shingle-lock/src/main.rs:661).
//!
//! The service file shipped for this is data/pam/shingle. The pre-rename
//! `tilefire` service and `TILEFIRE_PAM_SERVICE` variable are still honoured;
//! see README.md#upgrading-from-tilefire.

#![warn(missing_docs)]

use std::ffi::{OsStr, OsString};

use nonstick::{
  AuthnFlags, ConversationAdapter, ErrorCode, Result as PamResult, Transaction,
  TransactionBuilder,
};
use thiserror::Error;
use zeroize::Zeroizing;

/// A password that is zeroed in memory when dropped.
///
/// [`Debug`](std::fmt::Debug) prints `Password([redacted])` so the secret
/// cannot reach logs. [`Authenticator::authenticate`] takes it by value, so
/// it is wiped once the PAM transaction ends.
pub struct Password(Zeroizing<String>);

impl Password {
  /// Takes ownership of `password` without copying it. Pass an owned
  /// `String` rather than a `&str`: a borrowed string is copied, and the
  /// caller's original is not zeroed.
  pub fn new(password: impl Into<String>) -> Self {
    Self(Zeroizing::new(password.into()))
  }
}

impl std::fmt::Debug for Password {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str("Password([redacted])")
  }
}

/// Why [`Authenticator::authenticate`] did not succeed.
#[derive(Debug, Error)]
pub enum AuthError {
  /// A PAM call failed. `ErrorCode::AuthenticationError` is a wrong password
  /// and `ErrorCode::AccountExpired` or `ErrorCode::NewAuthTokRequired`
  /// come from account management after a correct one. The lock client
  /// treats any error as "denied" and stays locked.
  #[error("PAM authentication failed: {0}")]
  Pam(#[from] ErrorCode),
}

/// Checks a user's password against one PAM service.
///
/// [`Default`] picks `shingle`, or `tilefire` when only
/// `/etc/pam.d/tilefire` exists, because PAM silently falls back to the
/// deny-all `other` service when a service file is missing. The file system
/// is checked each time `default()` is called.
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
  /// Uses the PAM service `service`, the file name under `/etc/pam.d`. No
  /// check is made that the service exists; a missing one makes PAM use
  /// `other`, which normally denies everything.
  pub fn new(service: impl Into<String>) -> Self {
    Self {
      service: service.into(),
    }
  }

  /// Uses `SHINGLE_PAM_SERVICE`, then the legacy `TILEFIRE_PAM_SERVICE`,
  /// then [`Default`]. A variable that is set but not valid Unicode is
  /// skipped; one set to the empty string is used as is.
  pub fn from_env() -> Self {
    // TILEFIRE_PAM_SERVICE is the pre-rename name and is still honoured.
    std::env::var("SHINGLE_PAM_SERVICE")
      .or_else(|_| std::env::var("TILEFIRE_PAM_SERVICE"))
      .map(Self::new)
      .unwrap_or_default()
  }

  /// Runs `pam_authenticate` then `pam_acct_mgmt` for `username`.
  ///
  /// PAM's visible prompts are answered with `username` and its masked
  /// prompts with `password`; PAM error and info messages are discarded.
  /// Blocks until the PAM stack returns, which may include a deliberate
  /// delay after a failure, so do not call it on an event-loop thread.
  ///
  /// # Errors
  ///
  /// [`AuthError::Pam`] when starting the transaction, authentication or
  /// account management fails, for example a wrong password or an expired
  /// account.
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
