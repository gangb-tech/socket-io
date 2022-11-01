use std::{
    ops::{Deref, DerefMut},
    sync::Arc,
    time::Duration,
};

use crate::{
    socket::Socket as InnerSocket, AckId, ClientBuilder, Error, Event, Packet, Payload, Result,
};

use backoff::{backoff::Backoff, ExponentialBackoff, ExponentialBackoffBuilder};
use futures_util::future::BoxFuture;
use tokio::sync::RwLock;
use tracing::{trace, warn};

#[derive(Clone)]
pub struct Client {
    builder: ClientBuilder,
    socket: Arc<RwLock<InnerSocket<Socket>>>,
    backoff: ExponentialBackoff,
    connected: Arc<RwLock<bool>>,
}

#[derive(Clone)]
pub struct Socket {
    pub(crate) socket: InnerSocket<Self>,
}

impl From<InnerSocket<Socket>> for Socket {
    fn from(socket: InnerSocket<Socket>) -> Self {
        Self { socket }
    }
}

impl Client {
    /// Sends a message to the server using the underlying `engine.io` protocol.
    /// This message takes an event, which could either be one of the common
    /// events like "message" or "error" or a custom event like "foo". But be
    /// careful, the data string needs to be valid JSON. It's recommended to use
    /// a library like `serde_json` to serialize the data properly.
    ///
    /// # Example
    /// ```no_run
    /// use socketio_rs::{ClientBuilder, Socket, AckId, Payload};
    /// use serde_json::json;
    /// use futures_util::FutureExt;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let mut socket = ClientBuilder::new("http://localhost:4200/")
    ///         .on("test", |payload: Payload, socket: Socket, need_ack: Option<AckId>| {
    ///             async move {
    ///                 println!("Received: {:#?}", payload);
    ///                 socket.emit("test", json!({"hello": true})).await.expect("Server unreachable");
    ///             }.boxed()
    ///         })
    ///         .connect()
    ///         .await
    ///         .expect("connection failed");
    ///
    ///     let json_payload = json!({"token": 123});
    ///
    ///     let result = socket.emit("foo", json_payload).await;
    ///
    ///     assert!(result.is_ok());
    /// }
    /// ```
    #[inline]
    pub async fn emit<E, D>(&self, event: E, data: D) -> Result<()>
    where
        E: Into<Event>,
        D: Into<Payload>,
    {
        let socket = self.socket.read().await;
        socket.emit(event, data).await
    }

    /// Sends a message to the server but `alloc`s an `ack` to check whether the
    /// server responded in a given time span. This message takes an event, which
    /// could either be one of the common events like "message" or "error" or a
    /// custom event like "foo", as well as a data parameter. But be careful,
    /// in case you send a [`Payload::String`], the string needs to be valid JSON.
    /// It's even recommended to use a library like serde_json to serialize the data properly.
    /// It also requires a timeout `Duration` in which the client needs to answer.
    /// If the ack is acked in the correct time span, the specified callback is
    /// called. The callback consumes a [`Payload`] which represents the data send
    /// by the server.
    ///
    /// Please note that the requirements on the provided callbacks are similar to the ones
    /// for [`crate::asynchronous::ClientBuilder::on`].
    /// # Example
    /// ```no_run
    /// use socketio_rs::{ClientBuilder, Socket, Payload};
    /// use serde_json::json;
    /// use std::time::Duration;
    /// use std::thread::sleep;
    /// use futures_util::FutureExt;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let mut socket = ClientBuilder::new("http://localhost:4200/")
    ///         .on("foo", |payload: Payload, _, _| async move { println!("Received: {:#?}", payload) }.boxed())
    ///         .connect()
    ///         .await
    ///         .expect("connection failed");
    ///
    ///     let ack_callback = |message: Payload, socket: Socket, _| {
    ///         async move {
    ///             match message {
    ///                 Payload::String(str) => println!("{}", str),
    ///                 Payload::Binary(bytes) => println!("Received bytes: {:#?}", bytes),
    ///             }
    ///         }.boxed()
    ///     };    
    ///
    ///
    ///     let payload = json!({"token": 123});
    ///     socket.emit_with_ack("foo", payload, Duration::from_secs(2), ack_callback).await.unwrap();
    ///
    ///     sleep(Duration::from_secs(2));
    /// }
    /// ```
    #[inline]
    pub async fn emit_with_ack<F, E, D>(
        &self,
        event: E,
        data: D,
        timeout: Duration,
        callback: F,
    ) -> Result<()>
    where
        F: for<'a> std::ops::FnMut(Payload, Socket, Option<AckId>) -> BoxFuture<'static, ()>
            + 'static
            + Send
            + Sync,
        E: Into<Event>,
        D: Into<Payload>,
    {
        let socket = self.socket.read().await;
        socket.emit_with_ack(event, data, timeout, callback).await
    }

    pub async fn ack(&self, id: usize, data: Payload) -> Result<()> {
        let socket = self.socket.read().await;
        socket.ack(id, data).await
    }

    /// Disconnects from the server by sending a socket.io `Disconnect` packet. This results
    /// in the underlying engine.io transport to get closed as well.
    pub async fn disconnect(&self) -> Result<()> {
        trace!("client disconnect");
        let mut connected = self.connected.write().await;
        if !*connected {
            return Ok(());
        }
        *connected = false;
        self.disconnect_socket().await
    }

    async fn disconnect_socket(&self) -> Result<()> {
        let socket = self.socket.read().await;
        socket.disconnect().await
    }

    pub(crate) async fn new(builder: ClientBuilder) -> Result<Self> {
        let b = builder.clone();
        let socket = b.connect_socket().await?;
        let connected = Arc::new(RwLock::new(true));
        let backoff = ExponentialBackoffBuilder::new()
            .with_initial_interval(Duration::from_millis(builder.reconnect_delay_min))
            .with_max_interval(Duration::from_millis(builder.reconnect_delay_max))
            .build();

        let s = Self {
            builder,
            socket: Arc::new(RwLock::new(socket)),
            backoff,
            connected,
        };

        Ok(s)
    }

    async fn reconnect(&mut self) {
        let mut reconnect_attempts = 0;
        if self.builder.reconnect {
            loop {
                if let Some(max_reconnect_attempts) = self.builder.max_reconnect_attempts {
                    if reconnect_attempts > max_reconnect_attempts {
                        break;
                    }
                }
                reconnect_attempts += 1;

                if let Some(backoff) = self.backoff.next_backoff() {
                    trace!("reconnect backoff {:?}", backoff);
                    tokio::time::sleep(backoff).await;
                }

                trace!("client reconnect {}", reconnect_attempts);
                if self.do_reconnect().await.is_ok() {
                    break;
                }
            }
        }
    }

    async fn do_reconnect(&self) -> Result<()> {
        let new_socket = self.builder.clone().connect_socket().await?;
        let mut socket = self.socket.write().await;
        *socket = new_socket;
        Ok(())
    }

    pub(crate) fn poll_callback(&self) {
        let mut self_clone = self.clone();
        // Use thread to consume items in iterator in order to call callbacks
        tokio::spawn(async move {
            trace!("start poll_callback ");
            // tries to restart a poll cycle whenever a 'normal' error occurs,
            // it just panics on network errors, in case the poll cycle returned
            // `Result::Ok`, the server receives a close frame so it's safe to
            // terminate
            #[allow(clippy::for_loops_over_fallibles)]
            loop {
                let packet = self_clone.poll_packet().await;
                trace!("poll_callback packet {:?}", packet);
                if let Some(Err(Error::IncompleteResponseFromEngineIo(_))) = packet {
                    //TODO: logging error
                    let _ = self_clone.disconnect_socket().await;
                    self_clone.reconnect().await;
                }
                if !*self_clone.connected.read().await {
                    break;
                }
            }
            warn!("poll_callback exist");
        });
    }

    pub(crate) async fn poll_packet(&self) -> Option<Result<Packet>> {
        let socket = self.socket.read().await;
        socket.poll_packet().await
    }
}

impl Deref for Socket {
    type Target = InnerSocket<Self>;

    fn deref(&self) -> &Self::Target {
        &self.socket
    }
}

impl DerefMut for Socket {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.socket
    }
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use super::*;
    use crate::{
        test::socket_io_server, AckId, Client, ClientBuilder, Event, Packet, PacketType, Payload,
        Result, ServerBuilder, ServerSocket,
    };

    use bytes::Bytes;
    use futures_util::FutureExt;
    use serde_json::json;
    use tokio::time::sleep;
    use tracing::info;

    #[tokio::test]
    async fn test_client() -> Result<()> {
        // tracing_subscriber::fmt()
        //     .with_env_filter("engineio=trace,socketio=trace")
        //     .init();
        setup_server();

        socket_io_integration().await?;
        socket_io_builder_integration().await?;
        socket_io_builder_integration_iterator().await?;
        Ok(())
    }

    async fn socket_io_integration() -> Result<()> {
        let url = socket_io_server();

        let socket = ClientBuilder::new(url)
            .on("test", |msg, _, _| {
                async {
                    match msg {
                        Payload::String(str) => info!("Received string: {}", str),
                        Payload::Binary(bin) => info!("Received binary data: {:#?}", bin),
                    }
                }
                .boxed()
            })
            .connect()
            .await?;

        let payload = json!({"token": 123_i32});
        let result = socket
            .emit("test", Payload::String(payload.to_string()))
            .await;

        assert!(result.is_ok());

        let ack = socket
            .emit_with_ack(
                "test",
                Payload::String(payload.to_string()),
                Duration::from_secs(1),
                |message: Payload, socket: Socket, _| {
                    async move {
                        let result = socket
                            .emit(
                                "test",
                                Payload::String(json!({"got ack": true}).to_string()),
                            )
                            .await;
                        assert!(result.is_ok());

                        info!("Yehaa! My ack got acked?");
                        if let Payload::String(str) = message {
                            info!("Received string Ack");
                            info!("Ack data: {}", str);
                        }
                    }
                    .boxed()
                },
            )
            .await;
        assert!(ack.is_ok());

        sleep(Duration::from_secs(2)).await;

        assert!(socket.disconnect().await.is_ok());

        Ok(())
    }

    async fn socket_io_builder_integration() -> Result<()> {
        let url = socket_io_server();

        // test socket build logic
        let socket_builder = ClientBuilder::new(url);

        let socket = socket_builder
            .namespace("/admin")
            .opening_header("accept-encoding", "application/json")
            .on("test", |str, _, _| {
                async move { info!("Received: {:#?}", str) }.boxed()
            })
            .on("message", |payload, _, _| {
                async move { info!("{:#?}", payload) }.boxed()
            })
            .connect()
            .await?;

        assert!(socket.emit("message", json!("Hello World")).await.is_ok());

        assert!(socket
            .emit("binary", Bytes::from_static(&[46, 88]))
            .await
            .is_ok());

        assert!(socket
            .emit_with_ack(
                "binary",
                json!("pls ack"),
                Duration::from_secs(1),
                |payload, _, _| async move {
                    info!("Yehaa the ack got acked");
                    info!("With data: {:#?}", payload);
                }
                .boxed()
            )
            .await
            .is_ok());

        sleep(Duration::from_secs(2)).await;

        Ok(())
    }

    async fn socket_io_builder_integration_iterator() -> Result<()> {
        let url = socket_io_server();

        // test socket build logic
        let socket_builder = ClientBuilder::new(url);

        let socket = socket_builder
            .namespace("/admin")
            .opening_header("accept-encoding", "application/json")
            .on("test", |str, _, _| {
                async move { info!("Received: {:#?}", str) }.boxed()
            })
            .on("message", |payload, _, _| {
                async move { info!("Received binary {:#?}", payload) }.boxed()
            })
            .connect_client()
            .await?;

        assert!(socket.emit("message", json!("Hello World")).await.is_ok());

        assert!(socket
            .emit("binary", Bytes::from_static(&[46, 88]))
            .await
            .is_ok());

        assert!(socket
            .emit_with_ack(
                "binary",
                json!("pls ack"),
                Duration::from_secs(1),
                |payload, _, _| async move {
                    info!("Yehaa the ack got acked");
                    info!("With data: {:#?}", payload);
                }
                .boxed()
            )
            .await
            .is_ok());

        test_socketio_socket(socket, "/admin".to_owned()).await
    }

    async fn test_socketio_socket(socket: Client, nsp: String) -> Result<()> {
        // ignore connect packet
        let _: Option<Packet> = Some(socket.poll_packet().await.unwrap()?);

        let packet: Option<Packet> = Some(socket.poll_packet().await.unwrap()?);
        assert!(packet.is_some());

        let packet = packet.unwrap();

        assert_eq!(
            packet,
            Packet::new(
                PacketType::Event,
                nsp.clone(),
                Some("[\"test\",\"Hello from the test event!\"]".to_owned()),
                None,
                0,
                None
            )
        );
        let packet: Option<Packet> = Some(socket.poll_packet().await.unwrap()?);

        assert!(packet.is_some());

        let packet = packet.unwrap();
        assert_eq!(
            packet,
            Packet::new(
                PacketType::BinaryEvent,
                nsp.clone(),
                Some("\"test\"".to_owned()),
                None,
                1,
                Some(vec![Bytes::from_static(&[1, 2, 3])]),
            )
        );

        let cb = |message: Payload, _, _| {
            async {
                info!("Yehaa! My ack got acked?");
                if let Payload::String(str) = message {
                    info!("Received string ack");
                    info!("Ack data: {}", str);
                }
            }
            .boxed()
        };

        assert!(socket
            .emit_with_ack(
                "test",
                Payload::String("123".to_owned()),
                Duration::from_secs(10),
                cb
            )
            .await
            .is_ok());

        Ok(())
    }

    fn setup_server() {
        let echo_callback =
            move |_payload: Payload, socket: ServerSocket, _need_ack: Option<AckId>| {
                async move {
                    socket.join(vec!["room 1"]).await;
                    socket.emit_to(vec!["room 1"], "echo", json!("")).await;
                    socket.leave(vec!["room 1"]).await;
                }
                .boxed()
            };

        let client_ack = move |_payload: Payload, socket: ServerSocket, need_ack: Option<AckId>| {
            async move {
                if let Some(ack_id) = need_ack {
                    socket
                        .ack(ack_id, json!("ack to client"))
                        .await
                        .expect("success");
                }
            }
            .boxed()
        };

        let server_recv_ack =
            move |_payload: Payload, socket: ServerSocket, _need_ack: Option<AckId>| {
                async move {
                    socket
                        .emit("server_recv_ack", json!(""))
                        .await
                        .expect("success");
                }
                .boxed()
            };

        let trigger_ack = move |_message: Payload, socket: ServerSocket, _| {
            async move {
                socket.join(vec!["room 2"]).await;
                socket
                    .emit_to_with_ack(
                        vec!["room 2"],
                        "server_ask_ack",
                        json!(true),
                        Duration::from_millis(400),
                        server_recv_ack,
                    )
                    .await;
                socket.leave(vec!["room 2"]).await;
            }
            .boxed()
        };

        let connect_cb = move |_payload: Payload, socket: ServerSocket, _| {
            async move {
                socket
                    .emit("test", "Hello from the test event!")
                    .await
                    .expect("success");

                socket
                    .emit("test", Payload::Binary(Bytes::from_static(&[1, 2, 3])))
                    .await
                    .expect("success");
            }
            .boxed()
        };

        let url = socket_io_server();
        let server = ServerBuilder::new(url.port().unwrap())
            .on("/admin", "echo", echo_callback)
            .on("/admin", "client_ack", client_ack)
            .on("/admin", "trigger_server_ack", trigger_ack)
            .on("/admin", Event::Connect, connect_cb)
            .build();

        tokio::spawn(async move { server.serve().await });
    }
}
