use std::{collections::HashMap, fmt::Debug, ops::Deref, pin::Pin, sync::Arc, time::Duration};

use engineio_rs::Sid;
use futures_util::{future::BoxFuture, Stream, StreamExt};
use tokio::sync::RwLock;
use tracing::trace;

use crate::{
    ack::AckId,
    callback::Callback,
    error::Result,
    packet::Packet,
    server::server::Server,
    socket::{RawSocket, Socket},
    Event, Payload,
};

#[derive(Clone)]
pub struct Client {
    socket: Socket<Self>,
    server: Arc<Server>,
    sid: Sid,
}

impl Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("sid").field(&self.sid).finish()
    }
}

impl Client {
    pub(crate) fn new<T: Into<String>>(
        socket: RawSocket,
        namespace: T,
        sid: Sid,
        on: Arc<RwLock<HashMap<Event, Callback<Self>>>>,
        server: Arc<Server>,
    ) -> Self {
        let server_clone = server.clone();
        let sid_clone = sid.clone();
        let client = Socket::new(
            socket,
            namespace,
            on,
            Arc::new(move |c| Client {
                sid: sid_clone.clone(),
                socket: c,
                server: server_clone.clone(),
            }),
        );

        Self {
            sid,
            socket: client,
            server,
        }
    }

    pub(crate) async fn handle_connect(&self) {
        trace!("handle_connect");
        let _ = self.socket.handle_connect().await;
    }

    pub fn sid(&self) -> Sid {
        self.sid.clone()
    }

    pub fn namespace(&self) -> String {
        self.socket.nsp.clone()
    }

    pub async fn join<T: Into<String>>(&self, rooms: Vec<T>) {
        self.server
            .join(&self.socket.nsp, rooms, self.sid.clone())
            .await;
    }

    pub async fn leave(&self, rooms: Vec<&str>) {
        self.server.leave(&self.socket.nsp, rooms, &self.sid).await;
    }

    pub async fn emit_to<E, D>(&self, rooms: Vec<&str>, event: E, data: D)
    where
        E: Into<Event>,
        D: Into<Payload>,
    {
        self.server
            .emit_to(&self.socket.nsp, rooms, event, data)
            .await
    }

    pub async fn emit_to_with_ack<F, E, D>(
        &self,
        rooms: Vec<&str>,
        event: E,
        data: D,
        timeout: Duration,
        callback: F,
    ) where
        F: for<'a> std::ops::FnMut(Payload, Self, Option<AckId>) -> BoxFuture<'static, ()>
            + 'static
            + Send
            + Sync
            + Clone,
        E: Into<Event>,
        D: Into<Payload>,
    {
        self.server
            .emit_to_with_ack(&self.socket.nsp, rooms, event, data, timeout, callback)
            .await
    }
}

impl Deref for Client {
    type Target = Socket<Client>;
    fn deref(&self) -> &Self::Target {
        &self.socket
    }
}

impl Stream for Client {
    type Item = Result<Packet>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.socket.poll_next_unpin(cx)
    }
}
