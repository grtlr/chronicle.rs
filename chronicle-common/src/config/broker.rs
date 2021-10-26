// Copyright 2021 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use super::*;
use log::warn;
use paho_mqtt::{
    AsyncClient,
    CreateOptionsBuilder,
};
use reqwest::Client;
use serde_json::Value;
use std::{
    collections::HashSet,
    net::SocketAddr,
};
use url::Url;

/// Broker application config
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub struct BrokerConfig {
    /// MQTT addresses the broker will use as feed sources separated by type
    pub mqtt_brokers: HashMap<MqttType, HashSet<Url>>,
    /// Mqtt stream capacity
    pub mqtt_stream_capacity: usize,
    /// API endpoints the broker will use to request missing data
    pub api_endpoints: HashSet<Url>,
    /// Retries per api endpoint.
    pub retries_per_endpoint: usize,
    /// Retries per scylla query.
    pub retries_per_query: usize,
    /// Defines the total number of concurrent collectors and solidifiers
    pub collector_count: u8,
    /// Defines the total number of concurrent requester per collector
    pub requester_count: u8,
    /// The api endpoint request maximum timeout
    pub request_timeout_secs: u64,
    /// Used by Importer(s) and Syncer:
    /// - Importer(s) uses this to define the maximum number of concurrent milestone data and messages
    /// - Syncer(worker which fills gaps) uses this to define the maximum number of solidify requests/milestone data.
    pub parallelism: u8,
    /// Desired range of milestone indexes to sync if missing
    pub sync_range: Option<SyncRange>,
    /// Complete gaps interval in seconds
    pub complete_gaps_interval_secs: u64,
    /// Archive directory
    pub logs_dir: Option<String>,
    /// The maximum log file size
    pub max_log_size: Option<u64>,
}

/// Enumerated MQTT feed source type
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum MqttType {
    /// Receives Messages
    Messages,
    /// Receives Referenced notifications
    MessagesReferenced,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            collector_count: 10,
            requester_count: 10,
            request_timeout_secs: 5,
            parallelism: 25,
            retries_per_endpoint: 5,
            retries_per_query: 100,
            complete_gaps_interval_secs: 60 * 60,
            mqtt_stream_capacity: 10000,
            mqtt_brokers: hashmap! {
                MqttType::Messages => hashset![
                    url::Url::parse("tcp://api.hornet-0.testnet.chrysalis2.com:1883").unwrap(),
                    url::Url::parse("tcp://api.hornet-1.testnet.chrysalis2.com:1883").unwrap(),
                ],
                MqttType::MessagesReferenced => hashset![
                    url::Url::parse("tcp://api.hornet-0.testnet.chrysalis2.com:1883").unwrap(),
                    url::Url::parse("tcp://api.hornet-1.testnet.chrysalis2.com:1883").unwrap(),
                ]
            },
            api_endpoints: hashset![
                url::Url::parse("https://api.hornet-0.testnet.chrysalis2.com/api/v1").unwrap(),
                url::Url::parse("https://api.hornet-1.testnet.chrysalis2.com/api/v1").unwrap(),
            ]
            .into(),
            sync_range: Some(Default::default()),
            logs_dir: Some("chronicle/logs/".to_owned()),
            max_log_size: Some(4 * 1024 * 1024 * 1024),
        }
    }
}

impl BrokerConfig {
    /// Verify that the broker's config is valid
    pub async fn verify(&mut self) -> anyhow::Result<()> {
        for mqtt_broker in self.mqtt_brokers.values().flatten() {
            let random_id: u64 = rand::random();
            let create_opts = CreateOptionsBuilder::new()
                .server_uri(mqtt_broker.as_str())
                .client_id(&format!("{}|{}", "verifier", random_id))
                .persistence(None)
                .finalize();
            let _client = AsyncClient::new(create_opts)
                .map_err(|e| anyhow!("Error verifying mqtt broker {}: {}", mqtt_broker, e))?;
        }
        let client = Client::new();
        self.api_endpoints = self
            .api_endpoints
            .drain()
            .filter_map(|endpoint| Self::adjust_api_endpoint(endpoint))
            .collect();
        for endpoint in self.api_endpoints.iter() {
            Self::verify_endpoint(&client, endpoint).await?
        }
        let sync_range = self.sync_range.get_or_insert_with(|| SyncRange::default());
        if sync_range.from == 0 || sync_range.to == 0 {
            bail!("Error verifying sync from/to, zero provided!\nPlease provide non-zero milestone index");
        } else if sync_range.from >= sync_range.to {
            bail!("Error verifying sync from/to, greater or equal provided!\nPlease provide lower \"Sync range from\" milestone index");
        }
        Ok(())
    }
    /// Adjust IOTA api endpoint url and ensure it's correct or return None otherwise
    pub fn adjust_api_endpoint(endpoint: Url) -> Option<Url> {
        let path = endpoint.as_str();
        if path.is_empty() {
            warn!("Empty endpoint provided!");
            return None;
        }
        if !path.ends_with("/") {
            warn!("Endpoint provided without trailing slash: {}", endpoint);
            let new_endpoint = format!("{}/", path).parse();
            if let Ok(new_endpoint) = new_endpoint {
                return Some(new_endpoint);
            } else {
                warn!("Could not append trailing slash!");
                return None;
            }
        }
        Some(endpoint)
    }

    /// Verify if the IOTA api endpoint is active and correct
    pub async fn verify_endpoint(client: &Client, endpoint: &Url) -> anyhow::Result<()> {
        let res = client
            .get(
                endpoint
                    .join("info")
                    .map_err(|e| anyhow!("Error verifying endpoint {}: {}", endpoint, e))?,
            )
            .send()
            .await
            .map_err(|e| anyhow!("Error verifying endpoint {}: {}", endpoint, e))?;
        if !res.status().is_success() {
            let url = res.url().clone();
            let err = res.json::<Value>().await;
            bail!(
                "Error verifying endpoint \"{}\"\nRequest URL: \"{}\"\nResult: {:#?}",
                endpoint,
                url,
                err
            );
        }
        Ok(())
    }
}
