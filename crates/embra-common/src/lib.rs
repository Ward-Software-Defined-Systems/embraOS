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

/// tonic decode ceiling for the media RPCs (`PutMedia` requests carry up
/// to `MEDIA_UPLOAD_MAX` = 12 MiB raw; `GetMedia` responses stay under
/// it). tonic's default is 4 MiB; every hop that RECEIVES media bytes —
/// brain server, apid server, apid→brain client, embra-web→apid client,
/// embra-console→apid client — sets `.max_decoding_message_size(..)` to
/// this. Encoding limits default to unbounded and are left alone.
pub const GRPC_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
