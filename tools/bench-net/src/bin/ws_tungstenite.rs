//! 基准：tokio-tungstenite 回显服务端（生态标准实现）。

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:18092".into());
    let listener = TcpListener::bind(&addr).await?;
    println!("ws_tungstenite listening on {addr}");
    loop {
        let (stream, _) = listener.accept().await?;
        stream.set_nodelay(true)?;
        tokio::spawn(async move {
            let _ = handle(stream).await;
        });
    }
}

async fn handle(
    stream: tokio::net::TcpStream,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut ws = tokio_tungstenite::accept_async(stream).await?;
    while let Some(msg) = ws.next().await {
        match msg? {
            Message::Text(t) => ws.send(Message::Text(t)).await?,
            Message::Binary(b) => ws.send(Message::Binary(b)).await?,
            Message::Close(_) => break,
            _ => {}
        }
    }
    Ok(())
}
