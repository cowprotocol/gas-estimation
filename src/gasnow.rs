use super::{linear_interpolation, GasPrice, GasPriceEstimating, Transport};
use anyhow::{anyhow, Context, Result};
use futures::lock::Mutex;
use std::{
    convert::TryInto,
    future::Future,
    time::{Duration, Instant},
};

// Gas price estimation with https://www.gasnow.org/ , api at https://taichi.network/#gasnow .

const API_URI: &str = "https://www.gasnow.org/api/v3/gas/price";
const RATE_LIMIT: Duration = Duration::from_secs(15);

pub struct GasNowGasStation<T> {
    transport: T,
    last_response: Mutex<Option<CachedResponse>>,
}

struct CachedResponse {
    // The time at which the request was sent.
    time: Instant,
    // The result of the last response. Error isn't Clone so we store None in the error case.
    data: Option<Response>,
}

#[derive(Clone, Copy, Debug, Default, serde::Deserialize, PartialEq)]
pub struct Response {
    pub code: u32,
    pub data: ResponseData,
}

// gas prices in wei
#[derive(Clone, Copy, Debug, Default, serde::Deserialize, PartialEq)]
pub struct ResponseData {
    pub rapid: f64,
    pub fast: f64,
    pub standard: f64,
    pub slow: f64,
}

pub const RAPID: Duration = Duration::from_secs(15);
pub const FAST: Duration = Duration::from_secs(60);
pub const STANDARD: Duration = Duration::from_secs(300);
pub const SLOW: Duration = Duration::from_secs(600);

pub fn estimate_with_limits(
    _gas_limit: f64,
    time_limit: Duration,
    response: &ResponseData,
) -> Result<GasPrice> {
    let points: &[(f64, f64)] = &[
        (RAPID.as_secs_f64(), response.rapid),
        (FAST.as_secs_f64(), response.fast),
        (STANDARD.as_secs_f64(), response.standard),
        (SLOW.as_secs_f64(), response.slow),
    ];
    Ok(GasPrice {
        legacy: linear_interpolation::interpolate(time_limit.as_secs_f64(), points.try_into()?),
        ..Default::default()
    })
}

impl<T: Transport> GasNowGasStation<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            last_response: Default::default(),
        }
    }

    async fn gas_price_without_cache(&self) -> Result<Response> {
        self.transport
            .get_json(API_URI, Default::default())
            .await
            .context("failed to get gasnow gas price")
    }

    // Ensures that no requests are made faster than the rate limit by caching the previous
    // response. Errors are part of the cache.
    async fn gas_price_with_cache<Fut>(
        &self,
        now: Instant,
        fetch: impl FnOnce() -> Fut,
    ) -> Result<Response>
    where
        Fut: Future<Output = Result<Response>>,
    {
        // It is possible that while we wait to get the lock here another thread inserts a new cache
        // entry in which case the cache time can be in the future from now. So we have to use
        // checked_duration_since to catch this.
        let mut lock = self.last_response.lock().await;
        match lock.as_ref() {
            Some(cached) if now.saturating_duration_since(cached.time) < RATE_LIMIT => {
                match cached.data {
                    Some(response) => Ok(response),
                    None => Err(anyhow!(
                        "previous gasnow response was error and cache has not yet expired"
                    )),
                }
            }
            _ => {
                let result = fetch().await;
                *lock = Some(CachedResponse {
                    time: now,
                    data: result.as_ref().ok().copied(),
                });
                result
            }
        }
    }
}

#[async_trait::async_trait]
impl<T: Transport> GasPriceEstimating for GasNowGasStation<T> {
    async fn estimate_with_limits(&self, gas_limit: f64, time_limit: Duration) -> Result<GasPrice> {
        let response = self
            .gas_price_with_cache(Instant::now(), || self.gas_price_without_cache())
            .await?
            .data;
        estimate_with_limits(gas_limit, time_limit, &response)
    }
}

#[cfg(test)]
mod tests {
    use futures::FutureExt;

    use super::super::tests::{FutureWaitExt as _, TestTransport};
    use super::*;
    use std::future::{ready, Pending};

    fn panic_future() -> Pending<Result<Response>> {
        panic!()
    }

    #[test]
    fn interpolates() {
        let data = ResponseData {
            rapid: 4.0,
            fast: 3.0,
            standard: 2.0,
            slow: 1.0,
        };
        let result = estimate_with_limits(0., Duration::from_secs(20), &data).unwrap();
        assert!(result.legacy > 3.0 && result.legacy < 4.0);
    }

    #[test]
    fn cache_works_ok() {
        let gasnow = GasNowGasStation::new(TestTransport::default());
        let now = Instant::now();
        let response = Response {
            code: 0,
            ..Default::default()
        };

        // insert value into cache
        gasnow
            .gas_price_with_cache(now, || ready(Ok(response)))
            .wait()
            .unwrap();
        // panic_future isn't called
        assert_eq!(
            gasnow
                .gas_price_with_cache(now, panic_future)
                .wait()
                .unwrap(),
            response
        );

        // cache gets updated after expiry
        let now = now + RATE_LIMIT;
        let response = Response {
            code: 1,
            ..Default::default()
        };
        assert_eq!(
            gasnow
                .gas_price_with_cache(now, || ready(Ok(response)))
                .wait()
                .unwrap(),
            response
        );
        assert_eq!(
            gasnow
                .gas_price_with_cache(now, panic_future)
                .wait()
                .unwrap(),
            response
        );
    }

    #[test]
    fn cache_remembers_error() {
        let gasnow = GasNowGasStation::new(TestTransport::default());
        let now = Instant::now();

        assert!(gasnow
            .gas_price_with_cache(now, || ready(Err(anyhow!(""))))
            .wait()
            .is_err());
        // panic_future isn't called
        assert!(gasnow
            .gas_price_with_cache(now, panic_future)
            .wait()
            .is_err());
    }

    #[test]
    fn does_not_panic_if_now_is_old() {
        let gasnow = GasNowGasStation::new(TestTransport::default());
        let now = Instant::now();
        *gasnow.last_response.lock().now_or_never().unwrap() = Some(CachedResponse {
            time: now + Duration::from_secs(1),
            data: None,
        });
        gasnow
            .gas_price_with_cache(now, panic_future)
            .now_or_never()
            .unwrap()
            .unwrap_err();
    }

    // cargo test gasnow -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_request() {
        let gasnow = GasNowGasStation::new(TestTransport::default());
        loop {
            let before = Instant::now();
            let response = gasnow.estimate().wait();
            println!("{:?} in {} s", response, before.elapsed().as_secs_f32());
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}
