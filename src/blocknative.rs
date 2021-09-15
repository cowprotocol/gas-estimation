use super::{linear_interpolation, GasPriceEstimating, Transport};
use crate::CachedResponse;
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::{
    convert::TryInto,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::Mutex,
    task::{self, JoinHandle},
};

// Gas price estimation with https://www.blocknative.com/gas-estimator , api https://docs.blocknative.com/gas-platform#example-request .

const API_URI: &str = "https://api.blocknative.com/gasprices/blockprices";

const TIME_PER_BLOCK: Duration = Duration::from_secs(15);
const RATE_LIMIT: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct EstimatedPrice {
    confidence: f64,
    price: f64,
    max_priority_fee_per_gas: f64,
    max_fee_per_gas: f64,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct BlockPrice {
    estimated_prices: Vec<EstimatedPrice>,
}

#[derive(Debug, serde::Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct Response {
    block_prices: Vec<BlockPrice>,
}

struct Request<T> {
    transport: T,
    header: http::header::HeaderMap,
}

impl<T: Transport> Request<T> {
    async fn gas_price(&self) -> Result<Response> {
        self.transport
            .get_json(API_URI, self.header.clone())
            .await
            .context("failed to get blocknative gas price")
    }
}

pub struct BlockNative {
    cached_response: Arc<Mutex<Option<CachedResponse<Response>>>>,
    handle: JoinHandle<()>,
}

impl Drop for BlockNative {
    fn drop(&mut self) {
        self.handle.abort();
        self.cached_response = Arc::default();
    }
}

impl BlockNative {
    pub fn new<T: Transport + 'static>(transport: T, header: http::header::HeaderMap) -> Self {
        let cached_response: Arc<Mutex<Option<CachedResponse<Response>>>> = Default::default();
        let cached_response_clone = cached_response.clone();

        //spawn task for updating the cached response
        let handle = task::spawn(async move {
            let request = Request { transport, header };
            loop {
                if let Ok(response) = request.gas_price().await {
                    let mut data = cached_response_clone.lock().await;
                    *data = Some(CachedResponse {
                        time: Instant::now(),
                        data: Some(response),
                    });
                }
                tokio::time::sleep(RATE_LIMIT).await;
            }
        });

        Self {
            cached_response,
            handle,
        }
    }
}

#[async_trait::async_trait]
impl GasPriceEstimating for BlockNative {
    async fn estimate_with_limits(&self, _gas_limit: f64, time_limit: Duration) -> Result<f64> {
        let response = match self.cached_response.lock().await.as_ref() {
            Some(data) => data.data.clone().unwrap_or_default(),
            None => Default::default(),
        };

        estimate_with_limits(time_limit, response)
    }
}

fn estimate_with_limits(time_limit: Duration, mut response: Response) -> Result<f64> {
    if let Some(block) = response.block_prices.first_mut() {
        //need to sort by confidence since Blocknative API does not guarantee sorted response
        block
            .estimated_prices
            .sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap()); //change to total_cmp when stable

        //if confidence is 90%, point is calculated as 15s / (90% / 100%)
        let points = block
            .estimated_prices
            .iter()
            .map(|estimated_price| {
                (
                    TIME_PER_BLOCK.as_secs_f64() / (estimated_price.confidence / 100.0),
                    estimated_price.price,
                )
            })
            .collect::<Vec<(f64, f64)>>();

        return Ok(linear_interpolation::interpolate(
            time_limit.as_secs_f64(),
            points.as_slice().try_into()?,
        ));
    }

    return Err(anyhow!("no valid response exist"));
}

#[cfg(test)]
mod tests {
    use super::super::tests::TestTransport;
    use super::*;
    use serde_json::json;

    #[tokio::test]
    #[ignore]
    async fn real_request() {
        {
            let mut header = http::header::HeaderMap::new();
            header.insert(
                "AUTHORIZATION",
                http::header::HeaderValue::from_str(&std::env::var("BLOCKNATIVE_API_KEY").unwrap())
                    .unwrap(), //or replace with api_key
            );

            let blocknative = BlockNative::new(TestTransport::default(), header);

            let mut interval = tokio::time::interval(Duration::from_secs(3));
            for _ in 0..9 {
                interval.tick().await;

                let res = blocknative
                    .estimate_with_limits(0.0, Duration::from_secs(20))
                    .await
                    .unwrap_or_default();
                println!("res {}", res);
            }
        }

        //expect blocknative resources are dropped

        let mut interval = tokio::time::interval(Duration::from_secs(2));
        for i in 0..29 {
            interval.tick().await;
            println!("test {}", i);
        }
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
        let price = estimate_with_limits(Duration::from_secs(16), response.clone()).unwrap();
        assert_eq!(price, 98.76);
        let price = estimate_with_limits(Duration::from_secs(17), response.clone()).unwrap();
        assert_eq!(price, 97.84);
        let price = estimate_with_limits(Duration::from_secs(19), response.clone()).unwrap();
        assert_eq!(price, 96.90666666666667);
        let price = estimate_with_limits(Duration::from_secs(25), response).unwrap();
        assert_eq!(price, 96.0);
    }
}
