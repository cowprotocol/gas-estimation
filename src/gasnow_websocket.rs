use crate::{
    gasnow::{self, ResponseData},
    EstimatedGasPrice, GasPriceEstimating,
};
use anyhow::{bail, ensure, Result};
use futures::StreamExt;
use serde_json::Value;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::{error::Error as TungsteniteError, Message};
use url::Url;

pub const DEFAULT_URL: &str = "wss://etherchain.org/api/gasnow";
pub const RECONNECT_INTERVAL: Duration = Duration::from_secs(15);

/// Similar to GasNowGasStation but subscribes to their websocket api for updates instead of
/// manually polling.
/// When an estimate is requested the most recently received response is used. Unless it is older
/// than a configurable time in which case an error is returned.
/// To receive responses from the server a tokio task is spawned.
/// The connection automatically reconnects.
/// The struct can be cloned to listen to the same websocket connection.
#[derive(Clone, Debug)]
pub struct GasNowWebSocketGasStation {
    max_update_age: Duration,
    receiver: watch::Receiver<Option<(Instant, ResponseData)>>,
}

impl GasNowWebSocketGasStation {
    pub fn new(max_update_age: Duration) -> Self {
        Self::with_error_reporter(max_update_age, LogErrorReporter)
    }

    pub fn with_error_reporter(
        max_update_age: Duration,
        error_reporter: impl ErrorReporting,
    ) -> Self {
        let (sender, receiver) = watch::channel(None);
        tokio::spawn(receive_forever(
            DEFAULT_URL.parse().unwrap(),
            RECONNECT_INTERVAL,
            sender,
            max_update_age,
            Arc::new(error_reporter),
        ));
        Self {
            max_update_age,
            receiver,
        }
    }

    pub async fn wait_for_first_update(&mut self) {
        while (*self.receiver.borrow_and_update()).is_none() {
            // Unwrap because sender cannot have been dropped  while our receiver exists.
            self.receiver.changed().await.unwrap();
        }
    }
}

#[async_trait::async_trait]
impl GasPriceEstimating for GasNowWebSocketGasStation {
    async fn estimate_with_limits(
        &self,
        gas_limit: f64,
        time_limit: std::time::Duration,
    ) -> Result<EstimatedGasPrice> {
        if let Some((instant, response)) = *self.receiver.borrow() {
            ensure!(
                instant.elapsed() <= self.max_update_age,
                "last update more than {} s in the past",
                self.max_update_age.as_secs()
            );
            gasnow::estimate_with_limits(gas_limit, time_limit, &response)
        } else {
            bail!("did not receive first update yet");
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum JsonMessage {
    Update { data: ResponseData },
    Other(Value),
}

/// Exits when all receivers have been dropped.
/// Automatically reconnects the websocket on errors or if no message has been received within
/// max_update_interval.
async fn receive_forever(
    api: Url,
    reconnect_interval: Duration,
    sender: watch::Sender<Option<(Instant, ResponseData)>>,
    max_update_interval: Duration,
    error_reporter: Arc<dyn ErrorReporting>,
) {
    let work = async {
        loop {
            connect_and_receive_until_error(
                &api,
                &sender,
                max_update_interval,
                error_reporter.clone(),
            )
            .await;
            tokio::time::sleep(reconnect_interval).await;
        }
    };
    let is_closed = sender.closed();
    futures::pin_mut!(is_closed);
    futures::pin_mut!(work);
    futures::future::select(work, is_closed).await;
    tracing::debug!("exiting because all receivers have been dropped");
}

/// Returns on first error or if no message is received within max_update_interval.
async fn connect_and_receive_until_error(
    api: &Url,
    sender: &watch::Sender<Option<(Instant, ResponseData)>>,
    max_update_interval: Duration,
    error_reporter: Arc<dyn ErrorReporting>,
) {
    let (mut stream, _) = match tokio::time::timeout(
        max_update_interval,
        tokio_tungstenite::connect_async(api),
    )
    .await
    {
        Err(_) => {
            error_reporter.report_error(Error::ConnectionTimeOut);
            return;
        }
        Ok(Err(err)) => {
            error_reporter.report_error(Error::ConnectionFailure(err));
            return;
        }
        Ok(Ok(result)) => result,
    };
    loop {
        let message = match tokio::time::timeout(max_update_interval, stream.next()).await {
            Err(_) => {
                error_reporter.report_error(Error::StreamTimeOut);
                return;
            }
            Ok(None) => {
                tracing::info!("websocket stream closed");
                return;
            }
            // It is unclear which errors exactly cause the websocket to become unusable so we stop
            // on any.
            Ok(Some(Err(err))) => {
                error_reporter.report_error(Error::StreamFailure(err));
                return;
            }
            Ok(Some(Ok(message))) => message,
        };
        let json_message: Result<JsonMessage, _> = match &message {
            Message::Text(text) => serde_json::from_str(text),
            Message::Binary(binary) => serde_json::from_slice(binary),
            _ => continue,
        };
        let json_message = match json_message {
            Ok(response) => response,
            Err(err) => {
                let msg = match message {
                    Message::Text(text) => text,
                    Message::Binary(binary) => String::from_utf8_lossy(&binary).into_owned(),
                    _ => unreachable!(),
                };
                error_reporter.report_error(Error::JsonDecodeFailed(msg, err));
                continue;
            }
        };
        match json_message {
            JsonMessage::Update { data } => {
                tracing::debug!(?data, "received update");
                let _ = sender.send(Some((Instant::now(), data)));
            }
            JsonMessage::Other(value) => {
                tracing::warn!(?value, "received unexpected message");
            }
        }
    }
}

/// A trait for configuring error reporting for the WebSocket gas estimator.
pub trait ErrorReporting: Send + Sync + 'static {
    fn report_error(&self, err: Error);
}

/// A possible error to be reported.
pub enum Error {
    /// The WebSocket timed out establishing a connection to the remote service.
    ConnectionTimeOut,
    /// An unexpected error occured connecting to the remote service.
    ConnectionFailure(TungsteniteError),
    /// The WebSocket message stream timed out.
    StreamTimeOut,
    /// An unexpected error occured reading the WebSocket message stream.
    StreamFailure(TungsteniteError),
    /// An error occured decoding the JSON gas update data.
    JsonDecodeFailed(String, serde_json::Error),
}

/// The default error reporter that just logs the errors.
pub struct LogErrorReporter;

impl ErrorReporting for LogErrorReporter {
    fn report_error(&self, err: Error) {
        match err {
            Error::ConnectionTimeOut => {
                tracing::warn!("websocket connect timed out");
            }
            Error::ConnectionFailure(err) => {
                tracing::warn!(?err, "websocket connect failed");
            }
            Error::StreamTimeOut => {
                tracing::warn!("websocket stream timed out");
            }
            Error::StreamFailure(err) => {
                tracing::warn!(?err, "websocket stream failed");
            }
            Error::JsonDecodeFailed(msg, err) => {
                tracing::warn!(?err, ?msg, "decode failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::metadata::LevelFilter;

    #[ignore]
    #[tokio::test]
    async fn real() {
        tracing_subscriber::fmt()
            .with_max_level(LevelFilter::DEBUG)
            .init();
        let mut gas = GasNowWebSocketGasStation::new(Duration::from_secs(20));
        // Probably fails because first estimate hasn't been received yet.
        tracing::info!("{:?}", gas.estimate().await);
        gas.wait_for_first_update().await;
        // Succeeds
        tracing::info!("{:?}", gas.estimate().await.unwrap());
        tokio::time::sleep(Duration::from_secs(5)).await;
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            tracing::info!("{:?}", gas.estimate().await);
        }
    }
}
