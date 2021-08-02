use crate::{
    gasnow::{self, ResponseData},
    GasPriceEstimating,
};
use anyhow::{bail, ensure, Result};
use futures::StreamExt;
use serde_json::Value;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

pub const DEFAULT_URL: &str = "wss://www.gasnow.org/ws/gasprice";
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
        let (sender, receiver) = watch::channel(None);
        tokio::spawn(receive_forever(
            DEFAULT_URL.parse().unwrap(),
            RECONNECT_INTERVAL,
            sender,
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
    ) -> Result<f64> {
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
/// Automatically reconnects the websocket.
async fn receive_forever(
    api: Url,
    reconnect_interval: Duration,
    sender: watch::Sender<Option<(Instant, ResponseData)>>,
) {
    let work = async {
        loop {
            connect_and_receive_until_error(&api, &sender).await;
            tokio::time::sleep(reconnect_interval).await;
        }
    };
    let is_closed = sender.closed();
    futures::pin_mut!(is_closed);
    futures::pin_mut!(work);
    futures::future::select(work, is_closed).await;
    tracing::debug!("exiting because all receivers have been dropped");
}

/// Returns on first error.
async fn connect_and_receive_until_error(
    api: &Url,
    sender: &watch::Sender<Option<(Instant, ResponseData)>>,
) {
    let (mut stream, _) = match tokio_tungstenite::connect_async(api).await {
        Ok(result) => result,
        Err(err) => {
            tracing::error!(?err, "websocket connect failed");
            return;
        }
    };
    while let Some(item) = stream.next().await {
        let message = match item {
            Ok(message) => message,
            // It is unclear which errors exactly cause the websocket to become unusable so we stop
            // on any.
            Err(err) => {
                tracing::error!(?err, "websocket failed");
                return;
            }
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
                tracing::error!(?err, ?msg, "decode failed");
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
