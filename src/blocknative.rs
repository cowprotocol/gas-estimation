use super::{linear_interpolation, GasPriceEstimating, Transport};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::{convert::TryInto, time::Duration};

// Gas price estimation with https://www.blocknative.com/gas-estimator , api https://docs.blocknative.com/gas-platform#example-request .

const API_URI: &str = "https://api.blocknative.com/gasprices/blockprices";

const PERCENT_99: Duration = Duration::from_secs(15);
const PERCENT_95: Duration = Duration::from_secs(23);
const PERCENT_90: Duration = Duration::from_secs(30);
const PERCENT_80: Duration = Duration::from_secs(40);
const PERCENT_70: Duration = Duration::from_secs(50);

pub struct BlockNative<T> {
    transport: T,
    header: http::header::HeaderMap,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct EstimatedPrice {
    confidence: u64,
    price: f64,
    max_priority_fee_per_gas: f64,
    max_fee_per_gas: f64,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct BlockPrice {
    estimated_prices: Vec<EstimatedPrice>,
}

#[derive(Debug, serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Response {
    block_prices: Vec<BlockPrice>,
}

impl<T: Transport> BlockNative<T> {
    pub fn new(transport: T, header: http::header::HeaderMap) -> Self {
        Self { transport, header }
    }

    async fn gas_price(&self) -> Result<Response> {
        self.transport
            .get_json(API_URI, self.header.clone())
            .await
            .context("failed to get blocknative gas price")
    }
}

#[async_trait::async_trait]
impl<T: Transport> GasPriceEstimating for BlockNative<T> {
    async fn estimate_with_limits(&self, _gas_limit: f64, time_limit: Duration) -> Result<f64> {
        let response = self.gas_price().await?;
        estimate_with_limits(time_limit, response)
    }
}

fn estimate_with_limits(time_limit: Duration, mut response: Response) -> Result<f64> {
    if let Some(block) = response.block_prices.first_mut() {
        //need to sort by confidence since Blocknative API does not guarantee sorted response
        block
            .estimated_prices
            .sort_by(|a, b| a.confidence.cmp(&b.confidence));

        let points: &[(f64, f64)] = &[
            (
                PERCENT_99.as_secs_f64(),
                block.estimated_prices.pop().unwrap_or_default().price,
            ),
            (
                PERCENT_95.as_secs_f64(),
                block.estimated_prices.pop().unwrap_or_default().price,
            ),
            (
                PERCENT_90.as_secs_f64(),
                block.estimated_prices.pop().unwrap_or_default().price,
            ),
            (
                PERCENT_80.as_secs_f64(),
                block.estimated_prices.pop().unwrap_or_default().price,
            ),
            (
                PERCENT_70.as_secs_f64(),
                block.estimated_prices.pop().unwrap_or_default().price,
            ),
        ];

        return Ok(linear_interpolation::interpolate(
            time_limit.as_secs_f64(),
            points.try_into()?,
        ));
    }

    return Err(anyhow!("invalid response from blocknative"));
}

#[cfg(test)]
mod tests {
    use super::super::tests::TestTransport;
    use super::*;
    use serde_json::json;

    #[tokio::test]
    #[ignore]
    async fn real_request() {
        let mut header = http::header::HeaderMap::new();
        header.insert(
            "AUTHORIZATION",
            http::header::HeaderValue::from_str(&std::env::var("BLOCKNATIVE_API_KEY").unwrap())
                .unwrap(), //or replace with api_key
        );
        let blocknative = BlockNative::new(TestTransport::default(), header);
        let _response = blocknative.gas_price().await;
    }

    #[test]
    fn estimate_with_limits_test() {
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
        let response: Response = serde_json::from_value(json).unwrap();

        let price = estimate_with_limits(Duration::from_secs(10), response.clone()).unwrap();
        assert_eq!(price, 104.0);
        let price = estimate_with_limits(Duration::from_secs(20), response.clone()).unwrap();
        assert_eq!(price, 100.875);
        let price = estimate_with_limits(Duration::from_secs(35), response.clone()).unwrap();
        assert_eq!(price, 97.5);
        let price = estimate_with_limits(Duration::from_secs(45), response.clone()).unwrap();
        assert_eq!(price, 96.5);
        let price = estimate_with_limits(Duration::from_secs(55), response).unwrap();
        assert_eq!(price, 96.0);
    }
}
