//! Delay the tail of one synthetic one-column SELECT response. Record any
//! command sent before its final packet, without retaining SQL or credentials.
use sqlx::mysql::{MySqlConnectOptions, MySqlSslMode};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Notify,
    task::JoinHandle,
};

const CLIENT_DEPRECATE_EOF: u32 = 1 << 24;

#[derive(Default)]
struct State {
    armed: bool,
    stall_tail: bool,
    deprecate_eof: bool,
    phase: u8,
    early_commands: Vec<u8>,
    delayed_rows: usize,
}

pub struct CommandOrderProxy {
    state: Arc<Mutex<State>>,
    task: JoinHandle<()>,
    stalled: Arc<Notify>,
    pub options: MySqlConnectOptions,
}

impl CommandOrderProxy {
    pub async fn start(options: MySqlConnectOptions) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
        let port = listener.local_addr().unwrap().port();
        let backend = (options.get_host().to_owned(), options.get_port());
        let state = Arc::new(Mutex::new(State::default()));
        let shared = state.clone();
        let stalled = Arc::new(Notify::new());
        let signal = stalled.clone();
        let task = tokio::spawn(async move {
            let (client, _) = listener.accept().await.expect("accept test client");
            let server = TcpStream::connect(backend)
                .await
                .expect("connect test backend");
            proxy_session(client, server, shared, signal).await;
        });
        Self {
            state,
            task,
            stalled,
            options: options
                .host("127.0.0.1")
                .port(port)
                .ssl_mode(MySqlSslMode::Disabled),
        }
    }

    pub fn arm(&self) {
        self.state.lock().unwrap().armed = true;
    }

    pub fn stall_tail(&self) {
        self.state.lock().unwrap().stall_tail = true;
    }

    pub async fn wait_until_stalled(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.stalled.notified())
            .await
            .expect("response must reach stalled tail");
    }

    pub async fn assert_ordered(mut self) {
        // Wait for the proxy to observe all commands through the client's EOF.
        tokio::time::timeout(Duration::from_secs(5), &mut self.task)
            .await
            .expect("proxy must terminate")
            .expect("proxy task");
        let state = self.state.lock().unwrap();
        assert_eq!(state.delayed_rows, 1, "must exercise delayed response tail");
        assert!(
            state.early_commands.is_empty(),
            "commands sent before response completed: {:?}",
            state.early_commands
        );
    }
}

impl Drop for CommandOrderProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn packet(reader: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Vec<u8>> {
    let mut header = [0_u8; 4];
    reader.read_exact(&mut header).await?;
    let len = header[0] as usize | (header[1] as usize) << 8 | (header[2] as usize) << 16;
    let mut bytes = vec![0; len + 4];
    bytes[..4].copy_from_slice(&header);
    reader.read_exact(&mut bytes[4..]).await?;
    Ok(bytes)
}

async fn proxy_session(
    client: TcpStream,
    server: TcpStream,
    state: Arc<Mutex<State>>,
    stalled: Arc<Notify>,
) {
    let (mut cr, mut cw) = client.into_split();
    let (mut sr, mut sw) = server.into_split();
    let requests_state = state.clone();
    let requests = async move {
        let mut handshake = true;
        while let Ok(bytes) = packet(&mut cr).await {
            {
                let mut state = requests_state.lock().unwrap();
                if handshake {
                    assert!(bytes.len() >= 8);
                    let capabilities = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
                    state.deprecate_eof = capabilities & CLIENT_DEPRECATE_EOF != 0;
                    handshake = false;
                } else if bytes[3] == 0 && bytes.len() > 4 {
                    let command = bytes[4];
                    if state.phase != 0 {
                        state.early_commands.push(command);
                    }
                    if state.armed && command == 0x17 {
                        // COM_STMT_EXECUTE
                        state.armed = false;
                        state.phase = 1;
                    }
                }
            }
            if sw.write_all(&bytes).await.is_err() {
                break;
            }
        }
    };
    let replies = async move {
        while let Ok(bytes) = packet(&mut sr).await {
            let delay = {
                let mut state = state.lock().unwrap();
                // Only the explicitly armed, one-column/two-row SELECT is parsed.
                match state.phase {
                    0 => false,
                    1 => {
                        assert_eq!(bytes[4], 1, "one-column result header");
                        state.phase = 2;
                        false
                    }
                    2 => {
                        state.phase = if state.deprecate_eof { 4 } else { 3 };
                        false
                    }
                    3 => {
                        state.phase = 4;
                        false
                    }
                    4 => {
                        state.phase = 5;
                        false
                    } // First row is delivered immediately.
                    5 => {
                        state.phase = 6;
                        state.delayed_rows += 1;
                        true // Hold the second row and therefore the final EOF.
                    }
                    6 => {
                        assert_eq!(bytes[4], 0xfe, "final result packet");
                        state.phase = 0;
                        false
                    }
                    _ => unreachable!(),
                }
            };
            if delay {
                let stall = state.lock().unwrap().stall_tail;
                if stall {
                    stalled.notify_one();
                    std::future::pending::<()>().await;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if cw.write_all(&bytes).await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        _ = requests => {},
        _ = replies => {},
    }
}
