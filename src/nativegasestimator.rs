//! Native gas price estimator based on the https://github.com/zsfelfoldi/feehistory/blob/main/docs/feeOracle.md

use crate::GasPrice1559;

use super::{EstimatedGasPrice, GasPriceEstimating};
use anyhow::{Context, Result};
use primitive_types::U256;
use serde::Deserialize;
use serde::{de::Error, Deserializer};
use std::f64::consts::{E, PI};
use std::time::Duration;
use web3::Transport;

const SAMPLE_MIN_PERCENTILE: f64 = 10.0; // sampled percentile range of exponentially weighted baseFee history
const SAMPLE_MAX_PERCENTILE: f64 = 30.0;

const MAX_REWARD_PERCENTILE: usize = 20; // effective reward value to be selected from each individual block
const MIN_BLOCK_PERCENTILE: f64 = 40.0; // economical priority fee to be selected from sorted individual block reward percentiles
const MAX_BLOCK_PERCENTILE: f64 = 70.0; // urgent priority fee to be selected from sorted individual block reward percentiles

const MAX_TIME_FACTOR: f64 = 128.0; // highest timeFactor in the returned list of suggestions (power of 2)
const EXTRA_PRIORITY_FEE_RATIO: f64 = 0.25; // extra priority fee offered in case of expected baseFee rise
const FALLBACK_PRIORITY_FEE: f64 = 2e9; // priority fee offered when there are no recent transactions

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

pub struct NativeGasEstimator<T> {
    pub transport: T,
}

impl<T: Transport> NativeGasEstimator<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    // suggest_fee returns fee suggestion at the latest block
    // feeHistory API call without a reward percentile specified is cheap even with a light client backend because it
    // only needs block headers. Therefore we can afford to fetch a hundred blocks of base fee history in order to make
    // meaningful estimates on variable time scales.
    async fn suggest_fee(&self) -> Result<Vec<(f64, EstimatedGasPrice)>> {
        let params = vec![300.into(), "latest".into(), ().into()];
        let response = self.transport.execute("eth_feeHistory", params).await?;
        //println!("response: {:?}", response);
        let fee_history: EthFeeHistory =
            serde_json::from_value(response).context("deserialize failed")?;

        println!("fee_history: {:?}", fee_history);
        // Initialize
        let mut base_fee: Vec<U256> = vec![];
        let mut order: Vec<usize> = vec![];
        for i in 0..fee_history.base_fee_per_gas.len() {
            base_fee.push(fee_history.base_fee_per_gas[i]); //this can be copied
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

        let rewards = self
            .collect_rewards(
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

    async fn collect_rewards(
        &self,
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
                println!("params: {:?}", params);
                let response = self.transport.execute("eth_feeHistory", params).await?;
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
    rewards[index] as f64
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
impl<T> GasPriceEstimating for NativeGasEstimator<T>
where
    T: Transport + Send + Sync,
    <T as Transport>::Out: Send,
{
    async fn estimate_with_limits(
        &self,
        _gas_limit: f64,
        _time_limit: Duration,
    ) -> Result<EstimatedGasPrice> {
        let fees = self.suggest_fee().await?;

        Ok(fees.last().copied().unwrap_or_default().1) //todo
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::NativeGasEstimator;
    use std::time::Duration;

    #[tokio::test]
    async fn real_request() {
        //let transport = web3::transports::Http::new("http://staging-erigon.rinkeby.gnosisdev.com").unwrap();
        let transport = web3::transports::Http::new(
            "https://mainnet.infura.io/v3/3b497b3196e4468288eb5c7f239e86f4",
        )
        .unwrap();

        let native_gas_estimator = NativeGasEstimator::new(transport);

        let _result = native_gas_estimator
            .estimate_with_limits(0.0, Duration::from_secs(1))
            .await;
    }
}
