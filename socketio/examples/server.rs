use futures_util::FutureExt;
use serde_json::json;
use socketio::{Payload, ServerBuilder, ServerClient};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("engineio=trace,socketio=trace")
        .init();
    let callback = |_payload: Payload, socket: ServerClient, _| {
        async move {
            socket.join(vec!["room 1"]).await;
            let _ = socket
                .emit_to(vec!["room 1"], "echo", json!({"got ack": true}))
                .await;
        }
        .boxed()
    };
    let server = ServerBuilder::new(4209)
        .on("/admin", "foo", callback)
        .build();
    server.serve().await;
}
