// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT license.

use std::{env, time::Duration};

use async_channel::Receiver;
use async_stream::stream;
use async_trait::async_trait;
use chariott_common::error::{Error, ResultExt as _};
use futures::{stream::BoxStream, StreamExt as _};
use paho_mqtt::{
    AsyncClient, ConnectOptionsBuilder, CreateOptionsBuilder, Message, MQTT_VERSION_5, QOS_2, MessageBuilder,
};
use tracing::info;

#[async_trait]
pub trait Subscriber {
    type Message;
    type Topic;

    async fn subscribe<'a>(&'a self, topic: String) -> Result<BoxStream<'a, Self::Message>, Error>;
}

#[async_trait]
pub trait Publisher {
    type Message;
    type Topic;

    async fn publish(&self, topic: Self::Topic, message: Self::Message) -> Result<(), Error>;
}

pub struct MqttMessaging {
    client: AsyncClient,
    receiver: Receiver<Option<Message>>,
}

impl Drop for MqttMessaging {
    fn drop(&mut self) {
        // Best-effort disconnect.
        _ = self.client.disconnect(None).wait();
    }
}

impl MqttMessaging {
    pub async fn connect(client_id: String) -> Result<Self, Error> {
        const BROKER_URL_ENV_NAME: &str = "BROKER_URL";
        const DEFAULT_BROKER_URL: &str = "tcp://localhost:1883";
        const MQTT_CLIENT_BUFFER_SIZE: usize = 200;

        let host = env::var(BROKER_URL_ENV_NAME).unwrap_or_else(|_| DEFAULT_BROKER_URL.to_owned());
        // The client ID is used in conjunction with session persistence to
        // re-establish existing subscriptions on disconnect. TODO: if the
        // broker goes down and does not persist the session, the client must
        // reestablish the subscriptions.
        let client_id = format!("car-bridge-{client_id}");

        info!("Connecting client '{client_id}' to MQTT broker at '{host}'.");

        let mut client = AsyncClient::new(
            CreateOptionsBuilder::new()
                .mqtt_version(MQTT_VERSION_5)
                .server_uri(host)
                .client_id(client_id)
                .finalize(),
        )
        .map_err_with("Failed to create MQTT client.")?;

        // Get the stream before connecting the client, as otherwise messages
        // may be lost. If the Car Bridge restarts while the number of
        // outstanding messages is larger than the buffer size, messages may be
        // lost.

        let receiver = client.get_stream(MQTT_CLIENT_BUFFER_SIZE);

        client
            .connect(
                ConnectOptionsBuilder::new()
                    .mqtt_version(MQTT_VERSION_5)
                    .automatic_reconnect(Duration::from_secs(1), Duration::from_secs(60))
                    .finalize(),
            )
            .await
            .map_err_with("Could not connect to MQTT broker.")?;

        Ok(Self { client, receiver })
    }
}

#[async_trait]
impl Subscriber for MqttMessaging {
    type Message = Message;
    type Topic = String;

    async fn subscribe<'a>(&'a self, topic: String) -> Result<BoxStream<'a, Self::Message>, Error> {
        // C2D messages must be delivered with QOS 2, as we cannot assume that
        // the fulfill requests they contain are always idempotent.

        self.client
            .subscribe(topic.clone(), QOS_2)
            .await
            .map_err_with("Could not subscribe to topic for receiving C2D messages.")?;

        let mut receiver = self.receiver.clone();

        let s = stream! {
            while let Some(message) = receiver.next().await {
                if let Some(message) = message {
                    if topic == message.topic() {
                        yield message;
                    }
                }
                else {
                    // Automatic reconnect is used when connecting the
                    // `AsyncClient`.
                    info!("Connection temporarily lost. Attempting automatic reconnect.");
                }
            }
        };

        Ok(s.boxed())
    }
}

#[async_trait]
impl Publisher for MqttMessaging {
    type Message = MessageBuilder;
    type Topic = String;

    async fn publish(&self, topic: Self::Topic, message: Self::Message) -> Result<(), Error> {
        self.client.publish(message.topic(topic).finalize()).await.map_err_with("Error when publishing a response.")
    }
}
