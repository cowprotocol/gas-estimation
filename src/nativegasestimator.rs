//! Native gas price estimator based on the https://github.com/zsfelfoldi/feehistory/blob/main/docs/feeOracle.md

use super::{linear_interpolation, EstimatedGasPrice, GasPrice1559, GasPriceEstimating};
use anyhow::{anyhow, ensure, Result};
use std::{
    convert::TryInto,
    f64::consts::{E, PI},
    fmt::Debug,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::task::{self, JoinHandle};
use web3::{
    types::{BlockNumber, U256},
    Transport,
};

const CACHED_RESPONSE_VALIDITY: Duration = Duration::from_secs(60);

//rate limit of ethereum L1 nodes
const RATE_LIMIT: Duration = Duration::from_secs(5);

/// Parameters for Native gas price estimator algorithm
#[derive(Debug, Clone)]
pub struct Params {
    // sampled percentile range of exponentially weighted baseFee history
    pub sample_min_percentile: f64,
    // sampled percentile range of exponentially weighted baseFee history
    pub sample_max_percentile: f64,
    // effective reward value to be selected from each individual block
    pub max_reward_percentile: usize,
    // economical priority fee to be selected from sorted individual block reward percentiles
    pub min_block_percentile: f64,
    // urgent priority fee to be selected from sorted individual block reward percentiles
    pub max_block_percentile: f64,
    // highest timeFactor in the returned list of suggestions (power of 2)
    pub max_time_factor: f64,
    // extra priority fee offered in case of expected baseFee rise
    pub extra_priority_fee_ratio: f64,
    // a little extra to add to have a non-rounded value
    pub extra_priority_fee_boost: f64,
    // priority fee offered when there are no recent transactions
    pub fallback_priority_fee: f64,
    // a coefficient to multiply max_fee_per_gas with, in order to increase chances of transaction inclusion
    pub bump_cap_coefficient: f64,
    // number of blocks to consider for fee history calculation
    pub fee_history_blocks: u64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            sample_min_percentile: 10.0,
            sample_max_percentile: 30.0,
            max_reward_percentile: 20,
            min_block_percentile: 30.0,
            max_block_percentile: 60.0,
            max_time_factor: 128.0,
            extra_priority_fee_ratio: 0.25,
            extra_priority_fee_boost: 1559.0,
            fallback_priority_fee: 2e9,
            bump_cap_coefficient: 2.0,
            fee_history_blocks: 300,
        }
    }
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
    pub async fn new<T: Transport + Send + Sync + 'static>(
        transport: T,
        params: Option<Params>,
    ) -> Result<Self>
    where
        <T as Transport>::Out: std::marker::Send,
    {
        let cached_response: Arc<Mutex<CachedResponse>> = Default::default();
        let cached_response_clone = cached_response.clone();
        let params = params.unwrap_or_default();

        //do one calculation to initially populate cache before any request for gas price estimation is received from our users
        match suggest_fee(transport.clone(), &params).await {
            Ok(fees) => {
                // bump cap to be the ~ 2 x base_fee_per_gas (similar as BlockNative does)
                let fees = fees
                    .into_iter()
                    .map(|(time_limit, gas_price)| {
                        (time_limit, gas_price.bump_cap(params.bump_cap_coefficient))
                    })
                    .collect();

                *cached_response_clone.lock().unwrap() = CachedResponse {
                    time: Instant::now(),
                    data: fees,
                };
            }
            Err(err) => {
                tracing::warn!(?err, "failed to calculate initial fees");
                return Err(anyhow!("failed to calculate initial fees"));
            }
        }

        //spawn task for updating the cached response every RATE_LIMIT seconds
        let handle = task::spawn(async move {
            loop {
                tokio::time::sleep(RATE_LIMIT).await;
                match suggest_fee(transport.clone(), &params).await {
                    Ok(fees) => {
                        // bump cap to be the ~ 2 x base_fee_per_gas (similar as BlockNative does)
                        let fees = fees
                            .into_iter()
                            .map(|(time_limit, gas_price)| {
                                (time_limit, gas_price.bump_cap(params.bump_cap_coefficient))
                            })
                            .collect();

                        *cached_response_clone.lock().unwrap() = CachedResponse {
                            time: Instant::now(),
                            data: fees,
                        };
                    }
                    Err(err) => tracing::warn!(?err, "failed to calculate fees"),
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
// only needs block headers. Therefore we can afford to fetch high number of blocks of base fee history in order to make
// meaningful estimates on variable time scales.
async fn suggest_fee<T: Transport + Send + Sync>(
    transport: T,
    params: &Params,
) -> Result<Vec<(f64, EstimatedGasPrice)>> {
    let web3 = web3::Web3::new(transport.clone());
    let fee_history = web3
        .eth()
        .fee_history(
            params.fee_history_blocks.into(),
            serde_json::from_value::<BlockNumber>("latest".into()).unwrap(),
            None,
        )
        .await?;

    // Initialize
    let mut base_fee = fee_history.base_fee_per_gas.clone();
    let mut order = (0..fee_history.base_fee_per_gas.len()).collect::<Vec<_>>();

    // If a block is full then the baseFee of the next block is copied. The reason is that in full blocks the minimal
    // priority fee might not be enough to get included. The last (pending) block is also assumed to end up being full
    // in order to give some upwards bias for urgent suggestions.
    ensure!(
        fee_history.base_fee_per_gas.len() == fee_history.gas_used_ratio.len() + 1,
        "base_fee_per_gas not paired with gas_used_ratio"
    );
    base_fee[fee_history.base_fee_per_gas.len() - 1] *= 9 / 8;
    for (i, gas_ratio_used) in fee_history.gas_used_ratio.iter().enumerate().rev() {
        if *gas_ratio_used > 0.9 {
            base_fee[i] = base_fee[i + 1];
        }
    }

    order.sort_by(|a, b| base_fee[*a].cmp(&base_fee[*b]));

    let oldest_block = if let BlockNumber::Number(x) = fee_history.oldest_block {
        x.as_u64()
    } else {
        return Err(anyhow!("invalid oldest block"));
    };

    let rewards =
        collect_rewards(transport, oldest_block, fee_history.gas_used_ratio, params).await?;
    let mut result = vec![];
    let mut max_base_fee = 0.0;
    let mut time_factor = params.max_time_factor;
    while time_factor >= 1.0 {
        let priority_fee = suggest_priority_fee(&rewards, time_factor, params);
        let mut min_base_fee = predict_min_base_fee(&base_fee, &order, time_factor - 1.0, params);
        let mut extra_fee = 0.0;
        if min_base_fee > max_base_fee {
            max_base_fee = min_base_fee;
        } else {
            // If a narrower time window yields a lower base fee suggestion than a wider window then we are probably in a
            // price dip. In this case getting included with a low priority fee is not guaranteed; instead we use the higher
            // base fee suggestion and also offer extra priority fee to increase the chance of getting included in the base
            // fee dip.
            extra_fee = (max_base_fee - min_base_fee) * params.extra_priority_fee_ratio;
            min_base_fee = max_base_fee;
        }
        result.push((
            time_factor,
            EstimatedGasPrice {
                eip1559: Some(GasPrice1559 {
                    base_fee_per_gas: fee_history
                        .base_fee_per_gas
                        .last()
                        .copied()
                        .unwrap_or_default()
                        .low_u64() as f64,
                    max_fee_per_gas: min_base_fee + priority_fee,
                    max_priority_fee_per_gas: priority_fee + extra_fee,
                }),
                ..Default::default()
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
    params: &Params,
) -> Result<Vec<u64>> {
    let mut percentiles = vec![];
    for i in 0..=params.max_reward_percentile {
        percentiles.push(i as f64);
    }

    let mut ptr = gas_used_ratio.len() - 1;
    let mut need_blocks: usize = 5;
    let mut rewards = vec![];
    while need_blocks > 0 {
        let block_count = max_block_count(&gas_used_ratio, ptr, need_blocks)?;
        if block_count > 0 {
            // feeHistory API call with reward percentile specified is expensive and therefore is only requested for a few
            // non-full recent blocks.
            let web3 = web3::Web3::new(transport.clone());
            let fee_history = web3
                .eth()
                .fee_history(
                    block_count.into(),
                    (first_block + ptr as u64).into(),
                    Some(percentiles.clone()),
                )
                .await?;

            if fee_history.reward.is_none() {
                break;
            }

            let fee_history_reward = fee_history.reward.unwrap();
            for reward in &fee_history_reward {
                for i in 0..=params.max_reward_percentile {
                    let reward = reward[i].low_u64();
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

    rewards.sort_unstable();
    Ok(rewards)
}

// maxBlockCount returns the number of consecutive blocks suitable for priority fee suggestion (gasUsedRatio non-zero
// and not higher than 0.9).
fn max_block_count(gas_used_ratio: &[f64], last_index: usize, need_blocks: usize) -> Result<usize> {
    ensure!(
        gas_used_ratio.len() > last_index,
        "max_block_count invalid input"
    );
    Ok((0..std::cmp::min(last_index + 1, need_blocks))
        .into_iter()
        .take_while(|i| {
            !(gas_used_ratio[last_index - i] == 0.0 || gas_used_ratio[last_index - i] > 0.9)
        })
        .count())
}

// suggestPriorityFee suggests a priority fee (maxPriorityFeePerGas) value that's usually sufficient for blocks that
// are not full.
fn suggest_priority_fee(rewards: &[u64], time_factor: f64, params: &Params) -> f64 {
    if rewards.is_empty() {
        return params.fallback_priority_fee;
    }

    let factor = (params.min_block_percentile
        + (params.max_block_percentile - params.min_block_percentile) / time_factor)
        / 100.0;
    let index = ((rewards.len() - 1) as f64 * factor).floor() as usize;
    rewards[index] as f64 + params.extra_priority_fee_boost
}

// predictMinBaseFee calculates an average of base fees in the sampleMinPercentile to sampleMaxPercentile percentile
// range of recent base fee history, each block weighted with an exponential time function based on timeFactor.
fn predict_min_base_fee(base_fee: &[U256], order: &[usize], time_div: f64, params: &Params) -> f64 {
    if time_div < 1e-6 {
        return base_fee.last().copied().unwrap_or_default().low_u64() as f64;
    }

    let pending_weight =
        (1.0 - E.powf(-1.0 / time_div)) / (1.0 - E.powf(-(base_fee.len() as f64) / time_div));
    let mut sum_weight = 0.0;
    let mut result = 0.0;
    let mut sampling_curve_last = 0.0;
    for order_elem in order {
        sum_weight +=
            pending_weight * E.powf((*order_elem as f64 - base_fee.len() as f64 + 1.0) / time_div);
        let sampling_curve_value = sampling_curve(sum_weight * 100.0, params);
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
fn sampling_curve(percentile: f64, params: &Params) -> f64 {
    if percentile <= params.sample_min_percentile {
        return 0.0;
    }

    if percentile >= params.sample_max_percentile {
        return 1.0;
    }

    (1.0 - (((percentile - params.sample_min_percentile) * 2.0 * PI)
        / (params.sample_max_percentile - params.sample_min_percentile))
        .cos())
        / 2.0
}

#[async_trait::async_trait]
impl GasPriceEstimating for NativeGasEstimator {
    async fn estimate_with_limits(
        &self,
        _gas_limit: f64,
        time_limit: Duration,
    ) -> Result<EstimatedGasPrice> {
        let cached_response = self.cached_response.lock().unwrap().clone();

        estimate_with_limits(time_limit, cached_response)
    }
}

fn estimate_with_limits(
    time_limit: Duration,
    cached_response: CachedResponse,
) -> Result<EstimatedGasPrice> {
    if Instant::now().saturating_duration_since(cached_response.time) > CACHED_RESPONSE_VALIDITY {
        return Err(anyhow!("cached response is stale"));
    }

    if cached_response.data.is_empty() {
        return Err(anyhow!("no cached data exist"));
    }

    let max_fee_per_gas_points = cached_response
        .data
        .iter()
        .map(|(time_limit, gas_price)| {
            (
                *time_limit,
                gas_price.eip1559.unwrap_or_default().max_fee_per_gas,
            )
        })
        .collect::<Vec<(f64, f64)>>();
    let max_priority_fee_per_gas_points = cached_response
        .data
        .iter()
        .map(|(time_limit, gas_price)| {
            (
                *time_limit,
                gas_price
                    .eip1559
                    .unwrap_or_default()
                    .max_priority_fee_per_gas,
            )
        })
        .collect::<Vec<(f64, f64)>>();
    let base_fee_per_gas = if let Some(eip1559) = cached_response.data[0] //checked above
        .1
        .eip1559
    {
        eip1559.base_fee_per_gas
    } else {
        return Err(anyhow!("no eip1559 estimate exist"));
    };

    return Ok(EstimatedGasPrice {
        eip1559: Some(GasPrice1559 {
            max_fee_per_gas: linear_interpolation::interpolate(
                time_limit.as_secs_f64(),
                max_fee_per_gas_points.as_slice().try_into()?,
            ),
            max_priority_fee_per_gas: linear_interpolation::interpolate(
                time_limit.as_secs_f64(),
                max_priority_fee_per_gas_points.as_slice().try_into()?,
            ),
            base_fee_per_gas,
        }),
        ..Default::default()
    });
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

        let transport = web3::transports::Http::new(&std::env::var("NODE_URL").unwrap()).unwrap();

        //native gas estimator
        let native_gas_estimator = NativeGasEstimator::new(transport, None).await.unwrap();

        //blocknative gas estimator
        let mut header = http::header::HeaderMap::new();
        header.insert(
            "AUTHORIZATION",
            http::header::HeaderValue::from_str(&std::env::var("BLOCKNATIVE_API_KEY").unwrap())
                .unwrap(), //or replace with api_key
        );
        let blocknative = BlockNative::new(TestTransport::default(), header)
            .await
            .unwrap();

        //polling
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        for _ in 0..1000 {
            interval.tick().await;

            let res = native_gas_estimator
                .estimate_with_limits(0.0, Duration::from_secs(0))
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
        assert_approx_eq!(sampling_curve(0.0, &Default::default()), 0.0);
    }

    #[test]
    fn sampling_curve_maximum() {
        assert_approx_eq!(sampling_curve(100.0, &Default::default()), 1.0);
    }

    #[test]
    fn sampling_curve_expected() {
        assert_approx_eq!(sampling_curve(15.0, &Default::default()), 0.5);
    }

    #[test]
    fn max_block_count_test() {
        assert_eq!(
            max_block_count(&[0.5, 0.6, 0.7, 0.6, 0.5, 0.4, 0.4], 3, 5).unwrap(),
            4usize
        );
        assert_eq!(
            max_block_count(&[0.5, 0.6, 0.7, 0.6, 0.5, 0.4, 0.4], 4, 5).unwrap(),
            5usize
        );
        assert_eq!(
            max_block_count(&[0.5, 0.6, 0.7, 0.6, 0.5, 0.4, 0.4], 5, 5).unwrap(),
            5usize
        );

        assert_eq!(
            max_block_count(&[0.5, 0.6, 0.7, 0.6], 3, 5).unwrap(),
            4usize
        );
        assert_eq!(
            max_block_count(&[0.5, 0.6, 0.7, 0.6], 4, 5)
                .unwrap_err()
                .to_string(),
            "max_block_count invalid input"
        );
    }

    #[test]
    fn suggest_priority_fee_empty_rewards() {
        let params = Default::default();
        assert_approx_eq!(
            suggest_priority_fee(&[], 1.0, &params),
            params.fallback_priority_fee
        );
    }

    #[test]
    fn suggest_priority_fee_default_params() {
        let params = Default::default();
        assert_approx_eq!(
            suggest_priority_fee(
                &[
                    1000000000, 1110000000, 1213318421, 1433574636, 1557989644, 1615965689,
                    2000000000, 2557989644, 2910000000, 3000000000
                ],
                1.0,
                &params
            ),
            1615967248.0
        );
    }

    #[test]
    fn suggest_priority_fee_first_element() {
        let params = Params {
            min_block_percentile: 0.0,
            max_block_percentile: 0.0,
            extra_priority_fee_boost: 0.0,
            ..Default::default()
        };
        assert_approx_eq!(
            suggest_priority_fee(
                &[
                    1000000000, 1110000000, 1213318421, 1433574636, 1557989644, 1615965689,
                    2000000000, 2557989644, 2910000000, 3000000000
                ],
                1.0,
                &params
            ),
            1000000000.0
        );
    }

    #[test]
    fn suggest_priority_fee_middle_element() {
        let params = Params {
            min_block_percentile: 0.0,
            max_block_percentile: 50.0,
            extra_priority_fee_boost: 0.0,
            ..Default::default()
        };
        assert_approx_eq!(
            suggest_priority_fee(
                &[
                    1000000000, 1110000000, 1213318421, 1433574636, 1557989644, 1615965689,
                    2000000000, 2557989644, 2910000000, 3000000000
                ],
                1.0,
                &params
            ),
            1557989644.0
        );
    }

    #[test]
    fn suggest_priority_fee_last_element() {
        let params = Params {
            min_block_percentile: 0.0,
            max_block_percentile: 100.0,
            extra_priority_fee_boost: 0.0,
            ..Default::default()
        };
        assert_approx_eq!(
            suggest_priority_fee(
                &[
                    1000000000, 1110000000, 1213318421, 1433574636, 1557989644, 1615965689,
                    2000000000, 2557989644, 2910000000, 3000000000
                ],
                1.0,
                &params
            ),
            3000000000.0
        );
    }
}
