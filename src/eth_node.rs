//! Ethereum node `GasPriceEstimating` implementation.

use super::{EstimatedGasPrice, GasPriceEstimating};
use anyhow::{Context, Result};
use primitive_types::U256;
use std::time::Duration;
use web3::{Transport, Web3};

#[async_trait::async_trait]
impl<T> GasPriceEstimating for Web3<T>
where
    T: Transport + Send + Sync,
    <T as Transport>::Out: Send,
{
    async fn estimate_with_limits(
        &self,
        _gas_limit: f64,
        _time_limit: Duration,
    ) -> Result<EstimatedGasPrice> {
        let legacy = self
            .eth()
            .gas_price()
            .await
            .context("failed to get web3 gas price")
            .map(U256::to_f64_lossy)?;

        Ok(EstimatedGasPrice {
            legacy,
            ..Default::default()
        })
    }
}
