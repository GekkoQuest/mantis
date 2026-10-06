//! mantis-services: the service roles (plan 10) and the host library they
//! share.
//!
//! - [`host`]: roles, configuration, logging, metrics, health, typed RPC
//!   with timeouts, reconnect, and a caller matrix.
//! - [`methods`]: every RPC method and who may call it.

#![forbid(unsafe_code)]

pub mod generated {
    //! Generated from `schema/services.idl`.
    #[rustfmt::skip]
    pub mod services;
}

pub mod account;
pub mod cluster;
pub mod host;
pub mod inspect;
pub mod matchmaking;
pub mod methods;
pub mod ops;
pub mod persist;
pub mod realm;
pub mod social;
