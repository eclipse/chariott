// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT license.

use std::{sync::Arc, time::SystemTime};

use crate::proto::{
    common::Value as ValueMessage,
    common::{value::Value as ValueEnum, SubscribeFulfillment, SubscribeIntent},
    streaming::{channel_service_server::ChannelService, Event, OpenRequest},
};
use async_trait::async_trait;
use ess::EventSubSystem;
use tokio::spawn;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Response, Status};
use uuid::Uuid;

type InnerEss<T> = EventSubSystem<Box<str>, Box<str>, T, Result<Event, Status>>;

/// [`Ess`](Ess) (short for event sub-system) integrates the reusable
/// [`EventSubSystem`](EventSubSystem) component with the Chariott gRPC
/// streaming contract. Cloning [`Ess`](Ess) is cheap, it will not create a new
/// instance but refer to the same underlying instance instead.
#[derive(Clone)]
pub struct Ess<T>(Arc<InnerEss<T>>);

impl<T: Clone> Ess<T> {
    pub fn new() -> Self {
        Self(Arc::new(EventSubSystem::new()))
    }
}

impl<T: Clone> Default for Ess<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone + Send + 'static> Ess<T> {
    pub fn serve_subscriptions(
        &self,
        subscribe_intent: SubscribeIntent,
        into_value: fn(T) -> ValueEnum,
    ) -> Result<SubscribeFulfillment, Status> {
        let subscriptions = self
            .0
            .register_subscriptions(
                subscribe_intent.channel_id.into(),
                subscribe_intent.sources.into_iter().map(|s| s.into()),
            )
            .map_err(|_| Status::failed_precondition("The specified client does not exist."))?;

        for subscription in subscriptions {
            let source = subscription.event_id().to_string();

            spawn(subscription.serve(move |data, seq| {
                Ok(Event {
                    source: source.clone(),
                    value: Some(ValueMessage { value: Some(into_value(data)) }),
                    seq,
                    timestamp: Some(SystemTime::now().into()),
                })
            }));
        }

        Ok(SubscribeFulfillment {})
    }
}

#[async_trait]
impl<T> ChannelService for Ess<T>
where
    T: Clone + Send + Sync + 'static,
{
    type OpenStream = ReceiverStream<Result<Event, Status>>;

    async fn open(
        &self,
        _: tonic::Request<OpenRequest>,
    ) -> Result<Response<Self::OpenStream>, Status> {
        const METADATA_KEY: &str = "x-chariott-channel-id";

        let id = Uuid::new_v4().to_string();
        let (_, receiver_stream) = self.0.read_events(id.clone().into());
        let mut response = Response::new(receiver_stream);
        response.metadata_mut().insert(METADATA_KEY, id.try_into().unwrap());
        Ok(response)
    }
}

impl<T> AsRef<InnerEss<T>> for Ess<T> {
    fn as_ref(&self) -> &InnerEss<T> {
        self.0.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::proto::{
        common::Value as ValueMessage,
        common::{value::Value as ValueEnum, SubscribeIntent},
        streaming::{channel_service_server::ChannelService, OpenRequest},
    };
    use tokio_stream::{Stream, StreamExt as _};
    use tonic::{Code, Request};

    use super::Ess;

    #[tokio::test]
    async fn open_should_set_channel_id() {
        // arrange
        let subject = setup();

        // act
        let response = subject.open(Request::new(OpenRequest {})).await.unwrap();

        // assert
        assert!(!response.metadata().get("x-chariott-channel-id").unwrap().is_empty());
    }

    #[tokio::test]
    async fn serve_subscriptions_should_serve_subscription_for_event() {
        // arrange
        const EVENT_A: &str = "test-event-a";
        const EVENT_B: &str = "test-event-b";

        let subject = setup();
        let response = subject.open(Request::new(OpenRequest {})).await.unwrap();
        let channel_id =
            response.metadata().get("x-chariott-channel-id").unwrap().to_str().unwrap().into();

        // act
        subject
            .serve_subscriptions(
                SubscribeIntent { channel_id, sources: vec![EVENT_A.into(), EVENT_B.into()] },
                |_| ValueEnum::Null(0),
            )
            .unwrap();

        // assert
        subject.0.publish(EVENT_A, ());
        subject.0.publish(EVENT_B, ());

        let result = collect_when_stable(response.into_inner())
            .await
            .into_iter()
            .map(|e| e.unwrap())
            .collect::<Vec<_>>();

        // assert sources
        assert_eq!(
            vec![EVENT_A, EVENT_B],
            result.iter().map(|e| e.source.clone()).collect::<Vec<_>>()
        );

        // assert sequence numbers
        assert_eq!(1, result[0].seq);
        assert_eq!(1, result[1].seq);

        // assert payload
        assert_eq!(Some(ValueMessage { value: Some(ValueEnum::Null(0)) }), result[0].value);
    }

    #[tokio::test]
    async fn serve_subscriptions_should_error_when_no_client_active() {
        // arrange
        let subject = setup();

        // act
        let result = subject.serve_subscriptions(
            SubscribeIntent { channel_id: "client".into(), sources: vec!["test-event".into()] },
            |_| ValueEnum::Null(0),
        );

        // assert
        let result = result.unwrap_err();
        assert_eq!(Code::FailedPrecondition, result.code());
        assert_eq!("The specified client does not exist.", result.message());
    }

    fn setup() -> Ess<()> {
        Default::default()
    }

    async fn collect_when_stable<T>(stream: impl Stream<Item = T>) -> Vec<T> {
        static STABILIZATION_TIMEOUT: Duration = Duration::from_millis(100);
        stream
            .timeout(STABILIZATION_TIMEOUT)
            .take_while(|e| e.is_ok())
            .map(|e| e.unwrap())
            .collect()
            .await
    }
}
