//! Native gas price estimator based on the https://github.com/zsfelfoldi/feehistory/blob/main/docs/feeOracle.md

use crate::GasPrice1559;

use super::{EstimatedGasPrice, GasPriceEstimating};
use anyhow::{anyhow, Context, Result};
use serde::{de::Error, Deserialize, Deserializer};
use std::f64::consts::{E, PI};
use std::fmt::Debug;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::{self, JoinHandle};
use web3::{Transport, types::U256};

const SAMPLE_MIN_PERCENTILE: f64 = 10.0; // sampled percentile range of exponentially weighted baseFee history
const SAMPLE_MAX_PERCENTILE: f64 = 30.0;

const MAX_REWARD_PERCENTILE: usize = 80; // effective reward value to be selected from each individual block
const MIN_BLOCK_PERCENTILE: f64 = 20.0; // economical priority fee to be selected from sorted individual block reward percentiles
const MAX_BLOCK_PERCENTILE: f64 = 80.0; // urgent priority fee to be selected from sorted individual block reward percentiles

const MAX_TIME_FACTOR: f64 = 128.0; // highest timeFactor in the returned list of suggestions (power of 2)
const EXTRA_PRIORITY_FEE_RATIO: f64 = 0.25; // extra priority fee offered in case of expected baseFee rise
const EXTRA_PRIORITY_FEE_BOOST: f64 = 1559.0;
const FALLBACK_PRIORITY_FEE: f64 = 2e9; // priority fee offered when there are no recent transactions

//rate limit of ethereum L1 nodes
const RATE_LIMIT: Duration = Duration::from_secs(7);

#[derive(Debug)]
pub struct BlockNumber {
    block_number: u64,
}

impl BlockNumber {
    pub fn new(block_number: u64) -> Self {
        BlockNumber { block_number }
    }
}

impl<'a> Deserialize<'a> for BlockNumber {
    fn deserialize<D>(deserializer: D) -> Result<BlockNumber, D::Error>
    where
        D: Deserializer<'a>,
    {
        let value = String::deserialize(deserializer)?;
        match value {
            _ if value.starts_with("0x") => u64::from_str_radix(&value[2..], 16)
                .map(BlockNumber::new)
                .map_err(|e| Error::custom(format!("Invalid block number: {}", e))),
            _ => Err(Error::custom(
                "Invalid block number: missing 0x prefix".to_string(),
            )),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EthFeeHistory {
    pub oldest_block: BlockNumber,
    pub base_fee_per_gas: Vec<U256>,
    pub gas_used_ratio: Vec<f64>,
    pub reward: Option<Vec<Vec<U256>>>,
}

/// Used for rate limit implementation. If requests are received at a higher rate then Gas price estimators
/// can handle, we need to have a cached value that will be returned instead of error.
#[derive(Debug, Clone)]
pub struct CachedResponse {
    // The time at which the cache is last time updated.
    time: Instant,
    // List of gas price estimates, coupled with time_limit
    data: Vec<(f64, EstimatedGasPrice)>,
}

impl Default for CachedResponse {
    fn default() -> Self {
        Self {
            time: Instant::now(),
            data: Default::default(),
        }
    }
}

pub struct NativeGasEstimator {
    cached_response: Arc<Mutex<CachedResponse>>,
    handle: JoinHandle<()>,
}

impl Drop for NativeGasEstimator {
    fn drop(&mut self) {
        self.handle.abort();
        self.cached_response = Default::default();
    }
}

impl NativeGasEstimator {
    pub async fn new<T: Transport + Send + Sync + 'static>(transport: T) -> Result<Self>
    where
        <T as Transport>::Out: std::marker::Send,
    {
        let cached_response: Arc<Mutex<CachedResponse>> = Default::default();
        let cached_response_clone = cached_response.clone();

        //do one calculation to initially populate cache before any request for gas price estimation is received from our users
        match suggest_fee(transport.clone()).await {
            Ok(fees) => {
                // bump cap to be the ~ 2 x base_fee_per_gas (similar as BlockNative does)
                let fees = fees
                    .into_iter()
                    .map(|(time_limit, gas_price)| (time_limit, gas_price.bump_cap(2.0)))
                    .collect();

                *cached_response_clone.lock().unwrap() = CachedResponse {
                    time: Instant::now(),
                    data: fees,
                };
            }
            Err(e) => {
                tracing::warn!(?e, "failed to calculate initial fees");
                return Err(anyhow!("failed to calculate initial fees"));
            }
        }

        //spawn task for updating the cached response every RATE_LIMIT seconds
        let handle = task::spawn(async move {
            loop {
                tokio::time::sleep(RATE_LIMIT).await;
                match suggest_fee(transport.clone()).await {
                    Ok(fees) => {
                        // bump cap to be the ~ 2 x base_fee_per_gas (similar as BlockNative does)
                        let fees = fees
                            .into_iter()
                            .map(|(time_limit, gas_price)| (time_limit, gas_price.bump_cap(2.0)))
                            .collect();

                        *cached_response_clone.lock().unwrap() = CachedResponse {
                            time: Instant::now(),
                            data: fees,
                        };
                    }
                    Err(e) => tracing::warn!(?e, "failed to calculate fees"),
                }
            }
        });

        Ok(Self {
            cached_response,
            handle,
        })
    }
}

// suggest_fee returns fee suggestion at the latest block
// feeHistory API call without a reward percentile specified is cheap even with a light client backend because it
// only needs block headers. Therefore we can afford to fetch a hundred blocks of base fee history in order to make
// meaningful estimates on variable time scales.
async fn suggest_fee<T: Transport + Send + Sync>(
    transport: T,
) -> Result<Vec<(f64, EstimatedGasPrice)>> {
    let params = vec![300.into(), "latest".into(), ().into()];
    let response = transport.execute("eth_feeHistory", params).await?;
    let fee_history: EthFeeHistory =
        serde_json::from_value(response).context("deserialize failed")?;

    // Initialize
    let mut base_fee = fee_history.base_fee_per_gas.clone();
    let mut order: Vec<usize> = vec![];
    for i in 0..fee_history.base_fee_per_gas.len() {
        order.push(i);
    }

    // If a block is full then the baseFee of the next block is copied. The reason is that in full blocks the minimal
    // priority fee might not be enough to get included. The last (pending) block is also assumed to end up being full
    // in order to give some upwards bias for urgent suggestions.
    base_fee[fee_history.base_fee_per_gas.len() - 1] *= 9 / 8;
    for i in fee_history.gas_used_ratio.len() - 1..=0 {
        if fee_history.gas_used_ratio[i] > 0.9 {
            base_fee[i] = base_fee[i + 1];
        }
    }

    order.sort_by(|a, b| {
        base_fee[*a].cmp(&base_fee[*b]) //check if equivalent
    });

    let rewards = collect_rewards(
        transport,
        fee_history.oldest_block.block_number,
        fee_history.gas_used_ratio,
    )
    .await?;
    let mut result = vec![];
    let mut max_base_fee = 0.0;
    let mut time_factor = MAX_TIME_FACTOR;
    while time_factor >= 1.0 {
        let priority_fee = suggest_priority_fee(&rewards, time_factor);
        let mut min_base_fee = predict_min_base_fee(&base_fee, &order, time_factor - 1.0);
        let mut extra_fee = 0.0;
        if min_base_fee > max_base_fee {
            max_base_fee = min_base_fee;
        } else {
            // If a narrower time window yields a lower base fee suggestion than a wider window then we are probably in a
            // price dip. In this case getting included with a low priority fee is not guaranteed; instead we use the higher
            // base fee suggestion and also offer extra priority fee to increase the chance of getting included in the base
            // fee dip.
            extra_fee = (max_base_fee - min_base_fee) * EXTRA_PRIORITY_FEE_RATIO;
            min_base_fee = max_base_fee;
        }
        result.push((
            time_factor,
            EstimatedGasPrice {
                legacy: 0.0,
                eip1559: Some(GasPrice1559 {
                    base_fee_per_gas: fee_history
                        .base_fee_per_gas
                        .last()
                        .copied()
                        .unwrap_or_default()
                        .low_u64() as f64, //check
                    max_fee_per_gas: min_base_fee + priority_fee,
                    max_priority_fee_per_gas: priority_fee + extra_fee,
                }),
            },
        ));

        time_factor /= 2.0;
    }
    result.reverse();
    Ok(result)
}

async fn collect_rewards<T: Transport + Send + Sync>(
    transport: T,
    first_block: u64,
    gas_used_ratio: Vec<f64>,
) -> Result<Vec<u64>> {
    let mut percentiles = vec![];
    for i in 0..=MAX_REWARD_PERCENTILE {
        percentiles.push(i);
    }

    let mut ptr = gas_used_ratio.len() - 1;
    let mut need_blocks: usize = 5;
    let mut rewards = vec![];
    while need_blocks > 0 {
        let block_count = max_block_count(&gas_used_ratio, ptr, need_blocks);
        if block_count > 0 {
            // feeHistory API call with reward percentile specified is expensive and therefore is only requested for a few
            // non-full recent blocks.
            let params = vec![
                block_count.into(),
                format!("0x{:x}", first_block + ptr as u64).into(), //newest_block
                percentiles.clone().into(),
            ];
            let response = transport.execute("eth_feeHistory", params).await?;
            let fee_history: EthFeeHistory =
                serde_json::from_value(response).context("deserialize failed")?;

            if fee_history.reward.is_none() {
                break;
            }

            let fee_history_reward = fee_history.reward.unwrap();
            for i in 0..fee_history_reward.len() {
                for j in 0..=MAX_REWARD_PERCENTILE {
                    let reward = fee_history_reward[i][j].low_u64();
                    if reward > 0 {
                        rewards.push(reward);
                    }
                }
            }
            if fee_history_reward.len() < block_count {
                break;
            }

            need_blocks = need_blocks.saturating_sub(block_count);
        }

        if ptr < block_count + 1 {
            break;
        }
        ptr -= block_count + 1;
    }

    rewards.sort_unstable(); //check if needed asc or desc
    Ok(rewards)
}

// maxBlockCount returns the number of consecutive blocks suitable for priority fee suggestion (gasUsedRatio non-zero
// and not higher than 0.9).
fn max_block_count(
    gas_used_ratio: &Vec<f64>,
    mut last_index: usize,
    mut need_blocks: usize,
) -> usize {
    let mut block_count = 0;

    while need_blocks > 0 {
        if gas_used_ratio[last_index] == 0.0 || gas_used_ratio[last_index] > 0.9 {
            break;
        }

        block_count += 1;

        if last_index == 0 {
            break;
        }
        last_index -= 1;
        need_blocks -= 1;
    }
    block_count
}

// suggestPriorityFee suggests a priority fee (maxPriorityFeePerGas) value that's usually sufficient for blocks that
// are not full.
fn suggest_priority_fee(rewards: &Vec<u64>, time_factor: f64) -> f64 {
    if rewards.is_empty() {
        return FALLBACK_PRIORITY_FEE;
    }

    let factor = (MIN_BLOCK_PERCENTILE
        + (MAX_BLOCK_PERCENTILE - MIN_BLOCK_PERCENTILE) / time_factor)
        / 100.0;
    let index = ((rewards.len() - 1) as f64 * factor).floor() as usize;
    rewards[index] as f64 + EXTRA_PRIORITY_FEE_BOOST
}

// predictMinBaseFee calculates an average of base fees in the sampleMinPercentile to sampleMaxPercentile percentile
// range of recent base fee history, each block weighted with an exponential time function based on timeFactor.
fn predict_min_base_fee(base_fee: &Vec<U256>, order: &Vec<usize>, time_div: f64) -> f64 {
    if time_div < 1e6 {
        return base_fee.last().copied().unwrap_or_default().low_u64() as f64;
    }

    let pending_weight =
        (1.0 - E.powf(-1.0 / time_div)) / (1.0 - E.powf(-(base_fee.len() as f64) / time_div));
    let mut sum_weight = 0.0;
    let mut result = 0.0;
    let mut sampling_curve_last = 0.0;
    for order_elem in order {
        sum_weight += pending_weight * E.powf((order_elem - base_fee.len() + 1) as f64 / time_div);
        let sampling_curve_value = sampling_curve(sum_weight * 100.0);
        result +=
            (sampling_curve_value - sampling_curve_last) * base_fee[*order_elem].low_u64() as f64;
        if sampling_curve_value >= 1.0 {
            return result;
        }
        sampling_curve_last = sampling_curve_value;
    }
    result
}

// samplingCurve is a helper function for the base fee percentile range calculation.
fn sampling_curve(percentile: f64) -> f64 {
    if percentile <= SAMPLE_MIN_PERCENTILE {
        return 0.0;
    }

    if percentile >= SAMPLE_MAX_PERCENTILE {
        return 1.0;
    }

    (1.0 - (((percentile - SAMPLE_MIN_PERCENTILE) * 2.0 * PI)
        / (SAMPLE_MAX_PERCENTILE - SAMPLE_MIN_PERCENTILE))
        .cos())
        / 2.0
}

#[async_trait::async_trait]
impl GasPriceEstimating for NativeGasEstimator {
    async fn estimate_with_limits(
        &self,
        _gas_limit: f64,
        _time_limit: Duration,
    ) -> Result<EstimatedGasPrice> {
        let cached_response = self.cached_response.lock().unwrap().clone();

        Ok(cached_response.data.first().copied().unwrap_or_default().1) //todo
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::TestTransport;

    use super::*;

    use super::super::blocknative::BlockNative;
    use super::NativeGasEstimator;
    use assert_approx_eq::assert_approx_eq;
    use std::time::Duration;
    use std::{fs::File, io::Write};

    #[tokio::test]
    #[ignore]
    async fn real_request() {
        let mut file = File::create("foo.txt").unwrap();

        let transport = web3::transports::Http::new(
            "https://mainnet.infura.io/v3/3b497b3196e4468288eb5c7f239e86f4",
        )
        .unwrap();

        //native gas estimator
        let native_gas_estimator = NativeGasEstimator::new(transport).await.unwrap();

        //blocknative gas estimator
        let mut header = http::header::HeaderMap::new();
        header.insert(
            "AUTHORIZATION",
            http::header::HeaderValue::from_str("093e3f9c-bd8c-43c6-a6cd-3540fb3a2b70").unwrap(), //or replace with api_key
        );
        let blocknative = BlockNative::new(TestTransport::default(), header)
            .await
            .unwrap();

        //polling
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        for _ in 0..1000 {
            interval.tick().await;

            let res = native_gas_estimator
                .estimate_with_limits(0.0, Duration::from_secs(3))
                .await
                .unwrap_or_default();

            let serialized = serde_json::to_string(&res).unwrap() + "\n";
            file.write_all(serialized.as_bytes()).unwrap();
            println!("res written native");

            let res2 = blocknative
                .estimate_with_limits(0.0, Duration::from_secs(5))
                .await
                .unwrap_or_default();

            let serialized = serde_json::to_string(&res2).unwrap() + "\n";
            file.write_all(serialized.as_bytes()).unwrap();
            println!("res written blocknative");
        }
    }

    #[test]
    fn sampling_curve_minimum() {
        assert_eq!(sampling_curve(0.0), 0.0);
    }

    #[test]
    fn sampling_curve_maximum() {
        assert_eq!(sampling_curve(100.0), 1.0);
    }

    #[test]
    fn sampling_curve_expected() {
        assert_approx_eq!(sampling_curve(15.0), 0.5);
    }
}
