use std::{
    collections::HashMap,
    future::Future,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use reqwest_websocket::{Message, Upgrade};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

type Reply = oneshot::Sender<Result<Value, String>>;

#[derive(Clone)]
pub(super) struct Cdp(Arc<Inner>);

struct Inner {
    outgoing: mpsc::UnboundedSender<Message>,
    pending: Arc<Mutex<HashMap<u64, Reply>>>,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
    runtime: tokio::runtime::Handle,
    tasks: Vec<tokio::task::AbortHandle>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

struct Pending {
    id: u64,
    pending: Arc<Mutex<HashMap<u64, Reply>>>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.pending.lock().remove(&self.id);
    }
}

impl Cdp {
    pub(super) async fn connect(
        address: &str,
    ) -> anyhow::Result<(Self, mpsc::UnboundedReceiver<Value>)> {
        let url = url::Url::parse(address)?;
        anyhow::ensure!(
            url.scheme() == "ws" && url.host_str() == Some("127.0.0.1"),
            "CDP must use owned loopback endpoint"
        );
        let socket = reqwest::Client::builder()
            .no_proxy()
            .http1_only()
            .build()?
            .get(address)
            .upgrade()
            .send()
            .await?
            .into_websocket()
            .await?;
        let (mut sink, mut source) = socket.split();
        let (outgoing, mut commands) = mpsc::unbounded_channel();
        let (events, receiver) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<HashMap<u64, Reply>>> = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));
        let writer_closed = closed.clone();
        let writer_pending = pending.clone();
        let writer = tokio::spawn(async move {
            while let Some(message) = commands.recv().await {
                if sink.send(message).await.is_err() {
                    break;
                }
            }
            writer_closed.store(true, Ordering::Release);
            for (_, reply) in writer_pending.lock().drain() {
                let _ = reply.send(Err("Chrome CDP writer closed".into()));
            }
        });
        let reader_pending = pending.clone();
        let reader_closed = closed.clone();
        let reader = tokio::spawn(async move {
            while let Some(Ok(message)) = source.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    break;
                };
                if let Some(id) = value["id"].as_u64() {
                    if let Some(reply) = reader_pending.lock().remove(&id) {
                        let result = if value.get("error").is_some() {
                            Err(value["error"].to_string())
                        } else {
                            Ok(value["result"].clone())
                        };
                        let _ = reply.send(result);
                    }
                } else if events.send(value).is_err() {
                    break;
                }
            }
            reader_closed.store(true, Ordering::Release);
            for (_, reply) in reader_pending.lock().drain() {
                let _ = reply.send(Err("Chrome CDP connection closed".into()));
            }
        });
        Ok((
            Self(Arc::new(Inner {
                outgoing,
                pending,
                next_id: AtomicU64::new(1),
                closed,
                runtime: tokio::runtime::Handle::current(),
                tasks: vec![writer.abort_handle(), reader.abort_handle()],
            })),
            receiver,
        ))
    }

    pub(super) fn cleanup(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.0.runtime.spawn(future);
    }

    pub(super) fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::Acquire)
    }

    pub(super) async fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> anyhow::Result<Value> {
        anyhow::ensure!(!self.is_closed(), "Chrome CDP connection closed");
        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = session.into();
        }
        let (tx, rx) = oneshot::channel();
        self.0.pending.lock().insert(id, tx);
        let _pending = Pending {
            id,
            pending: self.0.pending.clone(),
        };
        self.0
            .outgoing
            .send(Message::Text(message.to_string().into()))
            .map_err(|_| anyhow::anyhow!("Chrome CDP writer unavailable"))?;
        tokio::time::timeout(Duration::from_secs(60), rx)
            .await??
            .map_err(anyhow::Error::msg)
    }
}
