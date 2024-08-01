//! Authentication backends.
//!
//! The `container-registry` supports pluggable authentication, as anything that implements the
//! [`AuthProvider`] trait can be used as an authentication (and authorization) backend. Included
//! are implementations for the following types:
//!
//! * `bool`: A simple always deny (`false`) / always allow (`true`) backend, mainly used in tests
//!           and example code. Will not accept missing credentials.
//! * `HashMap<String, Secret<String>>`: A mapping of usernames to (unencrypted) passwords.
//! * `Secret<String>`: Master password, ignores all usernames and just compares the password.
//!
//! All the above implementations deal with **authentication** only, once authorized, full
//! write access to everything is granted.
//!
//! To provide some safety against accidentally leaking passwords via stray `Debug` implementations,
//! this crate uses the [`sec`]'s crate [`Secret`] type.

use std::{any::Any, collections::HashMap, str, sync::Arc};

use axum::{
    async_trait,
    extract::FromRequestParts,
    http::{
        header::{self},
        request::Parts,
        StatusCode,
    },
};
use sec::Secret;

use crate::storage::ImageLocation;

use super::{
    www_authenticate::{self},
    ContainerRegistry,
};

/// A set of credentials supplied that has not been verified.
#[derive(Debug)]
pub enum Unverified {
    /// A set of username and password credentials.
    UsernameAndPassword {
        /// The given username.
        username: String,
        /// The provided password.
        password: Secret<String>,
    },
    /// No credentials were given.
    NoCredentials,
}

#[async_trait]
impl<S> FromRequestParts<S> for Unverified {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        if let Some(auth_header) = parts.headers.get(header::AUTHORIZATION) {
            let (_unparsed, basic) = www_authenticate::basic_auth_response(auth_header.as_bytes())
                .map_err(|_| StatusCode::BAD_REQUEST)?;

            Ok(Unverified::UsernameAndPassword {
                username: str::from_utf8(&basic.username)
                    .map_err(|_| StatusCode::BAD_REQUEST)?
                    .to_owned(),
                password: Secret::new(
                    str::from_utf8(&basic.password)
                        .map_err(|_| StatusCode::BAD_REQUEST)?
                        .to_owned(),
                ),
            })
        } else {
            Ok(Unverified::NoCredentials)
        }
    }
}

/// A set of credentials that has been validated.
#[derive(Debug)]
pub struct ValidCredentials(pub Box<dyn Any + Send + Sync>);

impl ValidCredentials {
    /// Creates a new set of valid credentials.
    #[inline(always)]
    fn new<T: Send + Sync + 'static>(inner: T) -> Self {
        ValidCredentials(Box::new(inner))
    }
}

#[async_trait]
impl FromRequestParts<Arc<ContainerRegistry>> for ValidCredentials {
    type Rejection = StatusCode;

    #[inline(always)]
    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<ContainerRegistry>,
    ) -> Result<Self, Self::Rejection> {
        let unverified = Unverified::from_request_parts(parts, state).await?;

        // We got a set of credentials, now verify.
        match state.auth_provider.check_credentials(&unverified).await {
            Some(creds) => Ok(creds),
            None => Err(StatusCode::UNAUTHORIZED),
        }
    }
}

/// A set of permissions granted on a specific image location to a given set of credentials.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Permissions {
    /// Access forbidden.
    NoAccess = 0,
    /// Write only access.
    WriteOnly = 2,
    /// Read access.
    Read = 4,
    /// Read and write access.
    ReadWrite = 6,
}

impl Permissions {
    /// Returns whether or not permissions include read access.
    #[inline(always)]
    pub fn permit_read(self) -> bool {
        match self {
            Permissions::NoAccess | Permissions::WriteOnly => false,
            Permissions::Read | Permissions::ReadWrite => true,
        }
    }

    /// Returns whether or not permissions include write access.
    #[inline(always)]
    pub fn permit_write(self) -> bool {
        match self {
            Permissions::NoAccess | Permissions::Read => false,
            Permissions::WriteOnly | Permissions::ReadWrite => true,
        }
    }
}

/// An authentication and authorization provider.
///
/// At the moment, `container-registry` gives full access to any valid user.
#[async_trait]
pub trait AuthProvider: Send + Sync {
    /// Checks whether the supplied unverified credentials are valid.
    ///
    /// Must return `None` if the credentials are not valid at all, malformed or similar.
    ///
    /// This is an **authenticating** function, returning `Some` indicates that the "login" was
    /// successful, but makes not statement about what these credentials can actually access (see
    /// `allowed_read()` and `allowed_write()` for authorization checks).
    async fn check_credentials(&self, unverified: &Unverified) -> Option<ValidCredentials>;

    /// Determine permissions for given credentials at image location.
    ///
    /// This is an **authorizing** function that determines permissions for previously authenticated
    /// credentials on a given [`ImageLocation`].
    async fn get_permissions(&self, creds: &ValidCredentials, image: &ImageLocation)
        -> Permissions;
}

#[async_trait]
impl AuthProvider for bool {
    #[inline(always)]
    async fn check_credentials(&self, _unverified: &Unverified) -> Option<ValidCredentials> {
        if *self {
            Some(ValidCredentials::new(()))
        } else {
            None
        }
    }

    #[inline(always)]
    async fn get_permissions(
        &self,
        _creds: &ValidCredentials,
        _image: &ImageLocation,
    ) -> Permissions {
        Permissions::ReadWrite
    }
}

#[async_trait]
impl AuthProvider for HashMap<String, Secret<String>> {
    async fn check_credentials(&self, unverified: &Unverified) -> Option<ValidCredentials> {
        match unverified {
            Unverified::UsernameAndPassword {
                username: unverified_username,
                password: unverified_password,
            } => {
                if let Some(correct_password) = self.get(unverified_username) {
                    if constant_time_eq::constant_time_eq(
                        correct_password.reveal().as_bytes(),
                        unverified_password.reveal().as_bytes(),
                    ) {
                        return Some(ValidCredentials::new(unverified_username.clone()));
                    }
                }

                None
            }
            Unverified::NoCredentials => None,
        }
    }

    #[inline(always)]
    async fn get_permissions(
        &self,
        _creds: &ValidCredentials,
        _image: &ImageLocation,
    ) -> Permissions {
        Permissions::ReadWrite
    }
}

#[async_trait]
impl<T> AuthProvider for Box<T>
where
    T: AuthProvider,
{
    #[inline(always)]
    async fn check_credentials(&self, unverified: &Unverified) -> Option<ValidCredentials> {
        <T as AuthProvider>::check_credentials(self, unverified).await
    }

    #[inline(always)]
    async fn get_permissions(
        &self,
        _creds: &ValidCredentials,
        _image: &ImageLocation,
    ) -> Permissions {
        Permissions::ReadWrite
    }
}

#[async_trait]
impl<T> AuthProvider for Arc<T>
where
    T: AuthProvider,
{
    #[inline(always)]
    async fn check_credentials(&self, unverified: &Unverified) -> Option<ValidCredentials> {
        <T as AuthProvider>::check_credentials(self, unverified).await
    }

    #[inline(always)]
    async fn get_permissions(
        &self,
        _creds: &ValidCredentials,
        _image: &ImageLocation,
    ) -> Permissions {
        Permissions::ReadWrite
    }
}

#[async_trait]
impl AuthProvider for Secret<String> {
    #[inline(always)]
    async fn check_credentials(&self, unverified: &Unverified) -> Option<ValidCredentials> {
        match unverified {
            Unverified::UsernameAndPassword {
                username: _,
                password,
            } => {
                if constant_time_eq::constant_time_eq(
                    password.reveal().as_bytes(),
                    self.reveal().as_bytes(),
                ) {
                    Some(ValidCredentials::new(()))
                } else {
                    None
                }
            }
            Unverified::NoCredentials => None,
        }
    }

    #[inline(always)]
    async fn get_permissions(
        &self,
        _creds: &ValidCredentials,
        _image: &ImageLocation,
    ) -> Permissions {
        Permissions::ReadWrite
    }
}
