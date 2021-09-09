use super::{GasPriceEstimating, Transport};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::time::Duration;

// Gas price estimation with https://www.blocknative.com/gas-estimator , api https://docs.blocknative.com/gas-platform#example-request .

const API_URI: &str = "https://api.blocknative.com/gasprices/blockprices";

pub struct BlockNative<T> {
    transport: T,
    api_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EstimatedPrice {
    confidence: u64,
    price: f64,
    max_priority_fee_per_gas: f64,
    max_fee_per_gas: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockPrice {
    estimated_prices: Vec<EstimatedPrice>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Response {
    block_prices: Vec<BlockPrice>,
}

impl<T: Transport> BlockNative<T> {
    pub fn new(transport: T, api_key: String) -> Self {
        Self { transport, api_key }
    }

    async fn gas_price(&self) -> Result<Response> {
        self.transport
            .get_json(API_URI, Some(self.api_key.clone()))
            .await
            .context("failed to get blocknative gas price")
    }
}

#[async_trait::async_trait]
impl<T: Transport> GasPriceEstimating for BlockNative<T> {
    async fn estimate_with_limits(&self, _gas_limit: f64, _time_limit: Duration) -> Result<f64> {
        let response = self.gas_price().await?;
        if let Some(block) = response.block_prices.first() {
            if let Some(estimated_price) = block.estimated_prices.first() {
                return Ok(estimated_price.price);
            }
        }
        return Err(anyhow!("invalid response from blocknative"));
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{FutureWaitExt as _, TestTransport};
    use super::*;
    use serde_json::json;

    #[test]
    #[ignore]
    fn real_request() {
        let blocknative = BlockNative::new(
            TestTransport::default(),
            std::env::var("BLOCKNATIVE_API_KEY").unwrap(), //or replace with api_key
        );
        let _response = blocknative.gas_price().wait().unwrap();
    }

    #[test]
    fn deserialize_response() {
        let json = json!({
          "system": "ethereum",
          "network": "main",
          "unit": "gwei",
          "maxPrice": "123",
          "currentBlockNumber": "13005095",
          "msSinceLastBlock": "3793",
          "blockPrices": [
            {
              "blockNumber": "13005096",
              "baseFeePerGas": "94.647990462",
              "estimatedTransactionCount": "137",
              "estimatedPrices": [
                {
                  "confidence": 99,
                  "price": 104,
                  "maxPriorityFeePerGas": 9.86,
                  "maxFeePerGas": 199.16
                },
                {
                  "confidence": 95,
                  "price": 99,
                  "maxPriorityFeePerGas": 5.06,
                  "maxFeePerGas": 194.35
                },
                {
                  "confidence": 90,
                  "price": 98,
                  "maxPriorityFeePerGas": 4.16,
                  "maxFeePerGas": 193.45
                },
                {
                  "confidence": 80,
                  "price": 97,
                  "maxPriorityFeePerGas": 2.97,
                  "maxFeePerGas": 192.27
                },
                {
                  "confidence": 70,
                  "price": 96,
                  "maxPriorityFeePerGas": 1.74,
                  "maxFeePerGas": 191.04
                }
              ]
            }
          ]
        });
        let _response: Response = serde_json::from_value(json).unwrap();
    }
}
