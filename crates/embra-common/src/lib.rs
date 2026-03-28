//! embra-common — Shared types and gRPC definitions for embraOS services.
//!
//! All inter-service communication uses types generated from the proto/ definitions.

pub mod proto {
    pub mod common {
        tonic::include_proto!("embra.common");
    }

    pub mod trust {
        tonic::include_proto!("embra.trust");
    }

    pub mod brain {
        tonic::include_proto!("embra.brain");
    }

    pub mod apid {
        tonic::include_proto!("embra.apid");
    }
}

// Re-export commonly used types at the crate root for convenience
pub use proto::common::{HealthCheckRequest, HealthCheckResponse, HealthStatus, SoulStatus, Timestamp};
