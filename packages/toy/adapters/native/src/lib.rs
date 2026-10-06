//! The toy package's native adapter: a thin configuration of the engine's
//! native adapter (plan 14). Predictive movement over QUIC, the native frame
//! envelope, and delta snapshots, all from the adapter contract.

#![forbid(unsafe_code)]

use mantis_adapter_contract::native::NativeAdapter;

/// The adapter's name in logs and configuration.
pub const NAME: &str = "toy.native";

/// The toy package's native adapter.
#[must_use]
pub const fn adapter() -> NativeAdapter {
    NativeAdapter::new(NAME)
}

#[cfg(test)]
mod tests {
    use mantis_adapter_contract::{MovementMode, TransportKind, WireAdapter};

    #[test]
    fn it_is_the_native_adapter_under_the_package_name() {
        let a = super::adapter();
        assert_eq!(a.name(), super::NAME);
        assert_eq!(a.movement_mode(), MovementMode::Predictive);
        assert_eq!(a.transport(), TransportKind::Quic);
    }
}
