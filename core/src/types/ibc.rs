//! IBC event without IBC-related data types

use std::cmp::Ordering;
use std::collections::HashMap;

use borsh::{BorshDeserialize, BorshSchema, BorshSerialize};

/// Wrapped IbcEvent
#[derive(
    Debug, Clone, BorshSerialize, BorshDeserialize, BorshSchema, PartialEq, Eq,
)]
pub struct IbcEvent {
    /// The IBC event type
    pub event_type: String,
    /// The attributes of the IBC event
    pub attributes: HashMap<String, String>,
}

impl std::cmp::PartialOrd for IbcEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.event_type.partial_cmp(&other.event_type)
    }
}

impl std::cmp::Ord for IbcEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        // should not compare the same event type
        self.event_type.cmp(&other.event_type)
    }
}

impl std::fmt::Display for IbcEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let attributes = self
            .attributes
            .iter()
            .map(|(k, v)| format!("{}: {};", k, v))
            .collect::<Vec<String>>()
            .join(", ");
        write!(
            f,
            "Event type: {}, Attributes: {}",
            self.event_type, attributes
        )
    }
}

#[cfg(any(feature = "abciplus", feature = "abcipp"))]
mod ibc_rs_conversion {
    use std::collections::HashMap;

    use thiserror::Error;

    use super::IbcEvent;
    use crate::ibc::core::events::{
        Error as IbcEventError, IbcEvent as RawIbcEvent,
    };
    use crate::tendermint_proto::abci::Event as AbciEvent;

    #[allow(missing_docs)]
    #[derive(Error, Debug)]
    pub enum Error {
        #[error("IBC event error: {0}")]
        IbcEvent(IbcEventError),
    }

    /// Conversion functions result
    pub type Result<T> = std::result::Result<T, Error>;

    impl TryFrom<RawIbcEvent> for IbcEvent {
        type Error = Error;

        fn try_from(e: RawIbcEvent) -> Result<Self> {
            let event_type = e.event_type().to_string();
            let abci_event = AbciEvent::try_from(e).map_err(Error::IbcEvent)?;
            let attributes: HashMap<_, _> = abci_event
                .attributes
                .iter()
                .map(|tag| (tag.key.to_string(), tag.value.to_string()))
                .collect();
            Ok(Self {
                event_type,
                attributes,
            })
        }
    }
}

#[cfg(any(feature = "abciplus", feature = "abcipp"))]
pub use ibc_rs_conversion::*;
