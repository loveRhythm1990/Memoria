//! Hold a PREPARE response at a chosen packet boundary until the client
//! disconnects. All queries are synthetic; no packet contents are retained.
use sqlx::mysql::{MySqlConnectOptions, MySqlSslMode};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Notify,
    task::JoinHandle,
};

const PAUSED: usize = usize::MAX;

struct Control {
    armed: AtomicBool,
    paused: Notify,
    disconnected: Notify,
    connections: AtomicUsize,
    commands_after_pause: AtomicUsize,
}

pub struct PreparePauseProxy {
    pub options: MySqlConnectOptions,
    control: Arc<Control>,
    task: JoinHandle<()>,
}

impl PreparePauseProxy {
    pub async fn start(options: MySqlConnectOptions, packets_before_pause: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
        let port = listener.local_addr().unwrap().port();
        let backend = (options.get_host().to_owned(), options.get_port());
        let control = Arc::new(Control {
            armed: AtomicBool::new(false),
            paused: Notify::new(),
            disconnected: Notify::new(),
            connections: AtomicUsize::new(0),
            commands_after_pause: AtomicUsize::new(0),
        });
        let shared = control.clone();
        let task = tokio::spawn(async move {
            let mut sessions = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (client, _) = accepted.expect("accept test connection");
                        let server = TcpStream::connect(backend.clone()).await.expect("connect test backend");
                        shared.connections.fetch_add(1, Ordering::SeqCst);
                        sessions.spawn(session(client, server, shared.clone(), packets_before_pause));
                    }
                    result = sessions.join_next(), if !sessions.is_empty() => {
                        result.unwrap().expect("proxy session");
                    }
                }
            }
        });
        Self {
            options: options
                .host("127.0.0.1")
                .port(port)
                .ssl_mode(MySqlSslMode::Disabled),
            control,
            task,
        }
    }

    pub async fn stall_prepare_write(options: MySqlConnectOptions) -> Self {
        Self::start(options, PAUSED).await
    }

    pub fn arm(&self) {
        self.control.armed.store(true, Ordering::SeqCst);
    }

    pub async fn paused(&self) {
        tokio::time::timeout(Duration::from_secs(2), self.control.paused.notified())
            .await
            .expect("PREPARE must reach pause boundary");
    }

    pub async fn assert_disconnected_without_more_commands(&self) {
        tokio::time::timeout(Duration::from_secs(2), self.control.disconnected.notified())
            .await
            .expect("cancelled PREPARE connection must disconnect");
        assert_eq!(
            self.control.commands_after_pause.load(Ordering::SeqCst),
            0,
            "must not send another command on the interrupted session"
        );
    }

    pub fn connection_count(&self) -> usize {
        self.control.connections.load(Ordering::SeqCst)
    }
}

impl Drop for PreparePauseProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn packet(
    reader: &mut (impl AsyncRead + Unpin),
    pause_write: Option<&Control>,
) -> std::io::Result<Vec<u8>> {
    let mut header = [0_u8; 4];
    reader.read_exact(&mut header).await?;
    let len = header[0] as usize | (header[1] as usize) << 8 | (header[2] as usize) << 16;
    let mut bytes = vec![0; len + 4];
    bytes[..4].copy_from_slice(&header);
    if len > 0 {
        reader.read_exact(&mut bytes[4..5]).await?;
        if header[3] == 0 && bytes[4] == 0x16 {
            if let Some(control) = pause_write {
                if control.armed.swap(false, Ordering::SeqCst) {
                    // Stop reading the request body so the client's send buffer
                    // fills. The test aborts this proxy after verifying replacement.
                    control.paused.notify_one();
                    std::future::pending::<()>().await;
                }
            }
        }
        reader.read_exact(&mut bytes[5..]).await?;
    }
    Ok(bytes)
}

async fn session(
    client: TcpStream,
    server: TcpStream,
    control: Arc<Control>,
    packets_before_pause: usize,
) {
    let (mut cr, mut cw) = client.into_split();
    let (mut sr, mut sw) = server.into_split();
    let phase = Arc::new(AtomicUsize::new(0));
    let requests_phase = phase.clone();
    let requests_control = control.clone();
    let requests = async move {
        let pause_write = (packets_before_pause == PAUSED).then_some(requests_control.as_ref());
        while let Ok(bytes) = packet(&mut cr, pause_write).await {
            if bytes[3] == 0 && bytes.len() > 4 {
                if requests_phase.load(Ordering::SeqCst) == PAUSED {
                    requests_control
                        .commands_after_pause
                        .fetch_add(1, Ordering::SeqCst);
                }
                if bytes[4] == 0x16 && requests_control.armed.swap(false, Ordering::SeqCst) {
                    requests_phase.store(1, Ordering::SeqCst);
                }
            }
            if sw.write_all(&bytes).await.is_err() {
                break;
            }
        }
    };
    let replies_phase = phase.clone();
    let replies_control = control.clone();
    let replies = async move {
        while let Ok(bytes) = packet(&mut sr, None).await {
            let phase = replies_phase.load(Ordering::SeqCst);
            if phase > 0 {
                if phase == packets_before_pause + 1 {
                    replies_phase.store(PAUSED, Ordering::SeqCst);
                    replies_control.paused.notify_one();
                    std::future::pending::<()>().await;
                }
                replies_phase.store(phase + 1, Ordering::SeqCst);
            }
            if cw.write_all(&bytes).await.is_err() {
                break;
            }
        }
    };
    tokio::select! { _ = requests => {}, _ = replies => {} }
    if phase.load(Ordering::SeqCst) == PAUSED {
        control.disconnected.notify_one();
    }
}
