use super::{linear_interpolation, GasPriceEstimating, Transport};
use anyhow::{anyhow, Result};
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
struct Response {
    code: u32,
    data: ResponseData,
}

// gas prices in wei
#[derive(Clone, Copy, Debug, Default, serde::Deserialize, PartialEq)]
struct ResponseData {
    rapid: f64,
    fast: f64,
    standard: f64,
    slow: f64,
}

const RAPID: Duration = Duration::from_secs(15);
const FAST: Duration = Duration::from_secs(60);
const STANDARD: Duration = Duration::from_secs(300);
const SLOW: Duration = Duration::from_secs(600);

impl<T: Transport> GasNowGasStation<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            last_response: Default::default(),
        }
    }

    async fn gas_price_without_cache(&self) -> Result<Response> {
        self.transport.get_json(API_URI).await
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
        let mut lock = self.last_response.lock().await;
        match lock.as_ref() {
            Some(cached) if now.duration_since(cached.time) < RATE_LIMIT => match cached.data {
                Some(response) => Ok(response),
                None => Err(anyhow!(
                    "previous response was error and cache has not yet expired"
                )),
            },
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
    async fn estimate_with_limits(&self, _gas_limit: f64, time_limit: Duration) -> Result<f64> {
        let response = self
            .gas_price_with_cache(Instant::now(), || self.gas_price_without_cache())
            .await?
            .data;
        let points: &[(f64, f64)] = &[
            (RAPID.as_secs_f64(), response.rapid),
            (FAST.as_secs_f64(), response.fast),
            (STANDARD.as_secs_f64(), response.standard),
            (SLOW.as_secs_f64(), response.slow),
        ];
        let result =
            linear_interpolation::interpolate(time_limit.as_secs_f64(), points.try_into()?);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{FutureWaitExt as _, TestTransport};
    use super::*;
    use std::future::{ready, Pending};

    fn panic_future() -> Pending<Result<Response>> {
        panic!()
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
