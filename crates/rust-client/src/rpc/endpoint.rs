use alloc::string::{String, ToString};
use core::fmt;

use miden_protocol::address::NetworkId;

// ENDPOINT
// ================================================================================================

/// The `Endpoint` struct represents a network endpoint, consisting of a protocol, a host, and a
/// port.
///
/// This struct is used to define the address of a Miden node that the client will connect to.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Endpoint {
    /// The protocol used to connect to the endpoint (e.g., "http", "https").
    protocol: String,
    /// The hostname or IP address of the endpoint.
    host: String,
    /// The port number of the endpoint.
    port: Option<u16>,
}

impl Endpoint {
    pub(crate) const MIDEN_NODE_PORT: u16 = 57291;

    /// Creates a new `Endpoint` with the specified protocol, host, and port.
    ///
    /// # Arguments
    ///
    /// * `protocol` - The protocol to use for the connection (e.g., "http", "https").
    /// * `host` - The hostname or IP address of the endpoint.
    /// * `port` - The port number to connect to.
    pub const fn new(protocol: String, host: String, port: Option<u16>) -> Self {
        Self { protocol, host, port }
    }

    /// Returns the [Endpoint] associated with the testnet network.
    pub fn testnet() -> Self {
        Self::new("https".into(), "rpc.testnet.miden.io".into(), None)
    }

    /// Returns the [Endpoint] associated with the devnet network.
    pub fn devnet() -> Self {
        Self::new("https".into(), "rpc.devnet.miden.io".into(), None)
    }

    /// Returns the [Endpoint] for a default node running in `localhost`.
    pub fn localhost() -> Self {
        Self::new("http".into(), "localhost".into(), Some(Self::MIDEN_NODE_PORT))
    }

    pub fn protocol(&self) -> &str {
        &self.protocol
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> Option<u16> {
        self.port
    }

    pub fn to_network_id(&self) -> NetworkId {
        if self == &Endpoint::testnet() {
            NetworkId::Testnet
        } else if self == &Endpoint::devnet() {
            NetworkId::Devnet
        } else if self == &Endpoint::localhost() {
            // Network ID intended to be used when running a local instance of the node
            NetworkId::new("mlcl").expect("mlcl should be a valid network ID")
        } else {
            // Default network ID for custom networks when no other match has been found
            NetworkId::new("mcst").expect("mcst should be a valid network ID")
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.port {
            Some(port) => write!(f, "{}://{}:{}", self.protocol, self.host, port),
            None => write!(f, "{}://{}", self.protocol, self.host),
        }
    }
}

impl Default for Endpoint {
    fn default() -> Self {
        Self::localhost()
    }
}

impl TryFrom<&str> for Endpoint {
    type Error = String;

    fn try_from(endpoint: &str) -> Result<Self, Self::Error> {
        let protocol_separator_index = endpoint.find("://");
        let port_separator_index = endpoint.rfind(':');

        // port separator index might match with the protocol separator, if so that means there was
        // no port defined
        let port_separator_index = if port_separator_index == protocol_separator_index {
            None
        } else {
            port_separator_index
        };

        let (protocol, hostname, port) = match (protocol_separator_index, port_separator_index) {
            (Some(protocol_idx), Some(port_idx)) => {
                let (protocol_and_hostname, port) = endpoint.split_at(port_idx);
                let port = port[1..]
                    .trim_end_matches('/')
                    .parse::<u16>()
                    .map_err(|err| err.to_string())?;

                let (protocol, hostname) = protocol_and_hostname.split_at(protocol_idx);
                // skip the separator
                let hostname = &hostname[3..];

                (protocol, hostname, Some(port))
            },
            (Some(protocol_idx), None) => {
                let (protocol, hostname) = endpoint.split_at(protocol_idx);
                // skip the separator
                let hostname = &hostname[3..];

                (protocol, hostname, None)
            },
            (None, Some(port_idx)) => {
                let (hostname, port) = endpoint.split_at(port_idx);
                let port = port[1..]
                    .trim_end_matches('/')
                    .parse::<u16>()
                    .map_err(|err| err.to_string())?;

                ("https", hostname, Some(port))
            },
            (None, None) => ("https", endpoint, None),
        };

        let hostname = hostname.trim_end_matches('/');

        if protocol.is_empty() {
            return Err("endpoint protocol cannot be empty".to_string());
        }

        if hostname.is_empty() {
            return Err("endpoint host cannot be empty".to_string());
        }

        Ok(Endpoint::new(protocol.to_string(), hostname.to_string(), port))
    }
}

#[cfg(test)]
mod test {
    use alloc::string::ToString;

    use crate::rpc::Endpoint;

    #[test]
    fn endpoint_parsing_with_hostname_only() {
        let endpoint = Endpoint::try_from("some.test.domain").unwrap();
        let expected_endpoint = Endpoint {
            protocol: "https".to_string(),
            host: "some.test.domain".to_string(),
            port: None,
        };

        assert_eq!(endpoint, expected_endpoint);
    }

    #[test]
    fn endpoint_parsing_with_ip() {
        let endpoint = Endpoint::try_from("192.168.0.1").unwrap();
        let expected_endpoint = Endpoint {
            protocol: "https".to_string(),
            host: "192.168.0.1".to_string(),
            port: None,
        };

        assert_eq!(endpoint, expected_endpoint);
    }

    #[test]
    fn endpoint_parsing_with_port() {
        let endpoint = Endpoint::try_from("some.test.domain:8000").unwrap();
        let expected_endpoint = Endpoint {
            protocol: "https".to_string(),
            host: "some.test.domain".to_string(),
            port: Some(8000),
        };

        assert_eq!(endpoint, expected_endpoint);
    }

    #[test]
    fn endpoint_parsing_with_ip_and_port() {
        let endpoint = Endpoint::try_from("192.168.0.1:8000").unwrap();
        let expected_endpoint = Endpoint {
            protocol: "https".to_string(),
            host: "192.168.0.1".to_string(),
            port: Some(8000),
        };

        assert_eq!(endpoint, expected_endpoint);
    }

    #[test]
    fn endpoint_parsing_with_protocol() {
        let endpoint = Endpoint::try_from("hkttp://some.test.domain").unwrap();
        let expected_endpoint = Endpoint {
            protocol: "hkttp".to_string(),
            host: "some.test.domain".to_string(),
            port: None,
        };

        assert_eq!(endpoint, expected_endpoint);
    }

    #[test]
    fn endpoint_parsing_with_protocol_and_ip() {
        let endpoint = Endpoint::try_from("http://192.168.0.1").unwrap();
        let expected_endpoint = Endpoint {
            protocol: "http".to_string(),
            host: "192.168.0.1".to_string(),
            port: None,
        };

        assert_eq!(endpoint, expected_endpoint);
    }

    #[test]
    fn endpoint_parsing_with_both_protocol_and_port() {
        let endpoint = Endpoint::try_from("http://some.test.domain:8080").unwrap();
        let expected_endpoint = Endpoint {
            protocol: "http".to_string(),
            host: "some.test.domain".to_string(),
            port: Some(8080),
        };

        assert_eq!(endpoint, expected_endpoint);
    }

    #[test]
    fn endpoint_parsing_with_ip_and_protocol_and_port() {
        let endpoint = Endpoint::try_from("http://192.168.0.1:8080").unwrap();
        let expected_endpoint = Endpoint {
            protocol: "http".to_string(),
            host: "192.168.0.1".to_string(),
            port: Some(8080),
        };

        assert_eq!(endpoint, expected_endpoint);
    }

    #[test]
    fn endpoint_parsing_should_fail_for_invalid_port() {
        let endpoint = Endpoint::try_from("some.test.domain:8000/hello");
        assert!(endpoint.is_err());
    }

    #[test]
    fn endpoint_parsing_should_fail_for_empty_protocol() {
        let endpoint = Endpoint::try_from("://some.test.domain:8000");
        assert!(endpoint.is_err());
    }

    #[test]
    fn endpoint_parsing_should_fail_for_empty_host() {
        let endpoint = Endpoint::try_from("https://:8000");
        assert!(endpoint.is_err());
    }

    #[test]
    fn endpoint_parsing_with_final_forward_slash() {
        let endpoint = Endpoint::try_from("https://some.test.domain:8000/").unwrap();
        let expected_endpoint = Endpoint {
            protocol: "https".to_string(),
            host: "some.test.domain".to_string(),
            port: Some(8000),
        };

        assert_eq!(endpoint, expected_endpoint);
    }

    #[test]
    fn endpoint_parsing_with_protocol_and_final_forward_slash_no_port() {
        let endpoint = Endpoint::try_from("http://some.test.domain/").unwrap();
        let expected_endpoint = Endpoint {
            protocol: "http".to_string(),
            host: "some.test.domain".to_string(),
            port: None,
        };

        assert_eq!(endpoint, expected_endpoint);
    }
}
