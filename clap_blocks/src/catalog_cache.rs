//! Config for the catalog cache server mode.

use std::time::Duration;

use itertools::Itertools;
use snafu::{OptionExt, Snafu};
use url::{Host, Url};

use crate::memory_size::MemorySize;

#[derive(Debug, Snafu)]
#[allow(missing_docs)]
pub enum Error {
    #[snafu(display("host '{host}' is not a prefix of '{prefix}'"))]
    NotAPrefix { host: String, prefix: String },

    #[snafu(display("host '{host}' is not a valid host"))]
    NotAValidHost { host: String },

    #[snafu(display("invalid url: {source}"))]
    InvalidUrl { source: url::ParseError },

    #[snafu(display("Expected exactly two peers"))]
    InvalidPeers,
}

/// CLI config for catalog configuration
#[derive(Debug, Clone, PartialEq, Eq, clap::Parser)]
pub struct CatalogConfig {
    /// Host Name
    ///
    /// If provided, any matching entries in peers will be ignored
    #[clap(long = "hostname", env = "INFLUXDB_IOX_HOSTNAME", value_parser = Host::parse)]
    pub hostname: Option<Host<String>>,

    /// Peers
    ///
    /// Can be provided as a comma-separated list, or on the command line multiple times
    #[clap(
        long = "catalog-cache-peers",
        env = "INFLUXDB_IOX_CATALOG_CACHE_PEERS",
        required = false,
        value_delimiter = ','
    )]
    pub peers: Vec<Url>,

    /// Peer connect timeout.
    #[clap(
        long = "catalog-cache-peer-connect-timeout",
        env = "INFLUXDB_IOX_CATALOG_CACHE_PEER_CONNECT_TIMEOUT",
        default_value = "2s",
        value_parser = humantime::parse_duration,
    )]
    pub peer_connect_timeout: Duration,

    /// Peer `GET` request timeout.
    #[clap(
        long = "catalog-cache-peer-get-request-timeout",
        env = "INFLUXDB_IOX_CATALOG_CACHE_PEER_GET_REQUEST_TIMEOUT",
        default_value = "1s",
        value_parser = humantime::parse_duration,
    )]
    pub peer_get_request_timeout: Duration,

    /// Peer `PUT` request timeout.
    #[clap(
        long = "catalog-cache-peer-put-request-timeout",
        env = "INFLUXDB_IOX_CATALOG_CACHE_PEER_PUT_REQUEST_TIMEOUT",
        default_value = "1s",
        value_parser = humantime::parse_duration,
    )]
    pub peer_put_request_timeout: Duration,

    /// Peer `LIST` request timeout.
    #[clap(
        long = "catalog-cache-peer-list-request-timeout",
        env = "INFLUXDB_IOX_CATALOG_CACHE_PEER_LIST_REQUEST_TIMEOUT",
        default_value = "20s",
        value_parser = humantime::parse_duration,
    )]
    pub peer_list_request_timeout: Duration,

    /// Warmup delay.
    ///
    /// The warm-up (via dumping the cache of our peers) is delayed by the given time to make sure that we already
    /// receive quorum writes. This ensure a gaplass transition / roll-out w/o any cache MISSes (esp. w/o any backend requests).
    #[clap(
        long = "catalog-cache-warmup-delay",
        env = "INFLUXDB_IOX_CATALOG_CACHE_WARMUP_DELAY",
        default_value = "5m",
        value_parser = humantime::parse_duration,
    )]
    pub warmup_delay: Duration,

    /// Garbage collection interval.
    ///
    /// Every time this interval past, cache elements that have not been used (i.e. read or updated) since the last time
    /// are evicted from the cache.
    #[clap(
        long = "catalog-cache-gc-interval",
        env = "INFLUXDB_IOX_CATALOG_CACHE_GC_INTERVAL",
        default_value = "15m",
        value_parser = humantime::parse_duration,
    )]
    pub gc_interval: Duration,

    /// Backoff when reacting to OOM situations.
    ///
    /// After triggering a garbage collection after an out-of-memory situation, the system will wait for the given
    /// amount of time before triggering the next OOM reaction.
    #[clap(
        long = "catalog-cache-oom-backoff",
        env = "INFLUXDB_IOX_CATALOG_CACHE_OOM_BACKOFF",
        default_value = "60s",
        value_parser = humantime::parse_duration,
    )]
    pub oom_backoff: Duration,

    /// Maximum number of bytes that should be cached within the catalog cache.
    ///
    /// If that limit is exceeded, no new values are accepted. This is meant as a safety measurement. You should adjust
    /// your pod size and the GC interval (`--catalog-cache-gc-interval` / `INFLUXDB_IOX_CATALOG_CACHE_GC_INTERVAL`) to
    /// your workload.
    ///
    /// Can be given as absolute value or in percentage of the total available memory (e.g. `10%`).
    #[clap(
        long = "catalog-cache-size-limit",
        env = "INFLUXDB_IOX_CATALOG_CACHE_SIZE_LIMIT",
        default_value = "1073741824",  // 1GB
        action
    )]
    pub cache_size_limit: MemorySize,

    /// Number of concurrent quorum operations that a single request can trigger.
    #[clap(
        long = "catalog-cache-quorum-fanout",
        env = "INFLUXDB_IOX_CATALOG_CACHE_QUORUM_FANOUT",
        default_value_t = 10
    )]
    pub quorum_fanout: usize,

    /// gRPC server timeout.
    #[clap(
        long = "catalog-cache-grpc-server-timeout",
        env = "INFLUXDB_IOX_CATALOG_CACHE_GRPC_SERVER_TIMEOUT",
        default_value = "20s",
        value_parser = humantime::parse_duration,
    )]
    pub grpc_server_timeout: Duration,
}

impl CatalogConfig {
    /// Return URL of other catalog cache nodes.
    pub fn peers(&self) -> Result<[Url; 2], Error> {
        let (peer1, peer2) = self
            .peers
            .iter()
            .filter(|x| match (x.host(), &self.hostname) {
                (Some(a), Some(r)) => &a != r,
                _ => true,
            })
            .collect_tuple()
            .context(InvalidPeersSnafu)?;

        Ok([peer1.clone(), peer2.clone()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_peers() {
        let config = CatalogConfig::parse_from([
            "binary",
            "--catalog-cache-peers",
            "http://peer1:8080",
            "--catalog-cache-peers",
            "http://peer2:9090",
        ]);
        let peer1 = Url::parse("http://peer1:8080").unwrap();
        let peer2 = Url::parse("http://peer2:9090").unwrap();

        let peers = config.peers().unwrap();
        assert_eq!(peers, [peer1.clone(), peer2.clone()]);

        let mut config = CatalogConfig::parse_from([
            "binary",
            "--catalog-cache-peers",
            "http://peer1:8080,http://peer2:9090,http://peer3:9091",
        ]);
        let err = config.peers().unwrap_err();
        assert!(matches!(err, Error::InvalidPeers), "{err}");

        config.hostname = Some(Host::parse("peer3").unwrap());
        let peers = config.peers().unwrap();
        assert_eq!(peers, [peer1.clone(), peer2.clone()]);
    }
}
