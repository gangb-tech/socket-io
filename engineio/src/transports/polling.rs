use std::{
    fmt::Debug,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use futures_util::{ready, FutureExt, Stream};
use http::HeaderMap;
use reqwest::{Client, ClientBuilder, Url};
use tokio::sync::{
    mpsc::{Receiver, Sender},
    Mutex,
};

use crate::{
    error::Result,
    transports::{append_hash, Payload, Transport},
    Error,
};

#[derive(Debug, Clone)]
pub struct ClientPolling {
    client: Client,
    url: Url,
}

#[derive(Debug, Clone)]
pub struct ServerPolling {
    sender: Arc<Sender<Bytes>>,
    receiver: Arc<Mutex<Receiver<Bytes>>>,
}

impl ClientPolling {
    pub fn new(mut url: Url, headers: Option<HeaderMap>) -> Result<Self> {
        let mut builder = ClientBuilder::new();
        if let Some(headers) = headers {
            builder = builder.default_headers(headers);
        }
        let client = builder.build()?;

        if !url
            .query_pairs()
            .any(|(k, v)| k == "transport" || v == "polling")
        {
            url.query_pairs_mut().append_pair("transport", "polling");
        }

        Ok(Self { client, url })
    }

    #[cfg(test)]
    pub(crate) fn url(&self) -> Url {
        self.url.clone()
    }
}

#[async_trait]
impl Transport for ClientPolling {
    async fn emit(&self, payload: Payload) -> Result<()> {
        let body = match payload {
            Payload::String(data) => data,
            Payload::Binary(data) => {
                let mut buf = BytesMut::with_capacity(data.len() + 1);
                buf.put_u8(b'b');
                buf.put(base64::encode(data).as_bytes());
                buf.freeze()
            }
        };

        let status = self
            .client
            .post(append_hash(&self.url))
            .body(body)
            .send()
            .await?
            .status()
            .as_u16();

        match status {
            200 => Ok(()),
            _ => Err(crate::Error::InvalidHttpResponseStatus(status)),
        }
    }
}

impl Stream for ClientPolling {
    type Item = Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match ready!(Box::pin(self.client.get(append_hash(&self.url)).send()).poll_unpin(cx)) {
            Ok(resp) => match ready!(Box::pin(resp.bytes()).poll_unpin(cx)) {
                Ok(bytes) => Poll::Ready(Some(Ok(bytes))),
                Err(e) => Poll::Ready(Some(Err(Error::HttpError(e)))),
            },
            Err(e) => Poll::Ready(Some(Err(Error::HttpError(e)))),
        }
    }
}

impl ServerPolling {
    fn new(sender: Sender<Bytes>, receiver: Receiver<Bytes>) -> Self {
        Self {
            sender: Arc::new(sender),
            receiver: Arc::new(Mutex::new(receiver)),
        }
    }
}

#[async_trait]
impl Transport for ServerPolling {
    async fn emit(&self, payload: Payload) -> Result<()> {
        let data = match payload {
            Payload::String(data) => data,
            Payload::Binary(data) => {
                let mut buf = BytesMut::with_capacity(data.len() + 1);
                buf.put_u8(b'b');
                buf.put(base64::encode(data).as_bytes());
                buf.freeze()
            }
        };

        self.sender.send(data).await.map_err(Error::SendError)?;
        Ok(())
    }
}

impl Stream for ServerPolling {
    type Item = Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut lock = ready!(Box::pin(self.receiver.lock()).poll_unpin(cx));
        let recv = ready!(Box::pin(lock.recv()).poll_unpin(cx));

        match recv {
            Some(bytes) => Poll::Ready(Some(Ok(bytes))),
            None => Poll::Ready(None),
        }
    }
}

#[cfg(test)]
mod test {
    use tokio::sync::mpsc::channel;

    use super::*;
    use futures_util::StreamExt;
    use std::str::FromStr;

    #[test]
    fn polling_transport_url() -> Result<()> {
        let url = Url::from_str("http://127.0.0.1").unwrap();
        let transport = ClientPolling::new(url.clone(), None).unwrap();
        assert_eq!(
            transport.url().to_string(),
            url.to_string() + "?transport=polling"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_server_polling_transport() -> Result<()> {
        let (send_tx, mut send_rx) = channel(100);
        let (recv_tx, recv_rx) = channel(100);
        let mut transport = ServerPolling::new(send_tx, recv_rx);

        let data = Bytes::from_static(b"1Hello\x1e1HelloWorld");

        recv_tx.send(data.clone()).await.unwrap();

        let msg = transport.next().await;
        assert!(msg.is_some());
        let msg = msg.unwrap();
        assert!(msg.is_ok());
        let msg = msg.unwrap();

        assert_eq!(msg, data);

        let payload = Payload::String(data.clone());
        transport.emit(payload).await?;
        let msg = send_rx.recv().await;
        assert!(msg.is_some());
        let msg = msg.unwrap();
        assert_eq!(msg, data);

        Ok(())
    }
}
