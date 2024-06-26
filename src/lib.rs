//! # Features
//! `web3_`: Implements `GasPriceEstimating` for `Web3`.

#[cfg(feature = "tokio_")]
pub mod blocknative;
#[cfg(feature = "web3_")]
pub mod eth_node;
pub mod gas_price;
pub mod gasnow;
mod linear_interpolation;
#[cfg(feature = "web3_")]
pub mod nativegasestimator;
pub mod priority;

#[cfg(feature = "tokio_")]
pub use blocknative::BlockNative;
pub use gas_price::GasPrice1559;
pub use gasnow::GasNowGasStation;
pub use priority::PriorityGasPriceEstimating;

use anyhow::Result;
use serde::de::DeserializeOwned;
use std::time::Duration;

pub const DEFAULT_GAS_LIMIT: f64 = 21000.0;
pub const DEFAULT_TIME_LIMIT: Duration = Duration::from_secs(30);

#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait GasPriceEstimating: Send + Sync {
    /// Estimate the gas price for a transaction to be mined "quickly".
    async fn estimate(&self) -> Result<GasPrice1559> {
        self.estimate_with_limits(DEFAULT_GAS_LIMIT, DEFAULT_TIME_LIMIT)
            .await
    }
    /// Estimate the gas price for a transaction that uses <gas> to be mined within <time_limit>.
    async fn estimate_with_limits(
        &self,
        gas_limit: f64,
        time_limit: Duration,
    ) -> Result<GasPrice1559>;
}

#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    async fn get_json<T: DeserializeOwned>(
        &self,
        url: &str,
        header: http::header::HeaderMap,
    ) -> Result<T>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    #[derive(Default)]
    pub struct TestTransport {}

    #[async_trait::async_trait]
    impl Transport for TestTransport {
        async fn get_json<T: DeserializeOwned>(
            &self,
            url: &str,
            header: http::header::HeaderMap,
        ) -> Result<T> {
            let json = reqwest::Client::new()
                .get(url)
                .headers(header)
                .send()
                .await?
                .text()
                .await?;

            Ok(serde_json::from_str(&json)?)
        }
    }

    pub trait FutureWaitExt: Future + Sized {
        fn wait(self) -> Self::Output {
            futures::executor::block_on(self)
        }
    }
    impl<F> FutureWaitExt for F where F: Future {}
}
