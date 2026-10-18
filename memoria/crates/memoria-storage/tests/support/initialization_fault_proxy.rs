//! Fault injection for synthetic initialization SQL. Forward DDL to the real
//! backend and hold its OK packet, or reject initialization/cleanup separately.
//! Neither SQL nor connection credentials are written to logs.
use sqlx::mysql::{MySqlConnectOptions, MySqlSslMode};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::Notify,
    task::JoinHandle,
};

#[path = "mysql_packet.rs"]
mod mysql_packet;
use mysql_packet::packet;

const COM_QUERY: u8 = 0x03;
const COM_STMT_PREPARE: u8 = 0x16;
const COM_STMT_EXECUTE: u8 = 0x17;

struct Pause {
    verb: &'static str,
    fragment: &'static str,
    release: Arc<Notify>,
}

#[derive(Default)]
struct Control {
    pause: Mutex<Option<Pause>>,
    release: Mutex<Option<Arc<Notify>>>,
    paused: Notify,
    reject_init: AtomicBool,
    reject_cleanup: AtomicBool,
    cleanup_rejections: AtomicUsize,
    pause_cleanup: AtomicBool,
    cleanup_paused: Notify,
    resume_cleanup: Notify,
}

pub struct InitializationFaultProxy {
    pub options: MySqlConnectOptions,
    control: Arc<Control>,
    task: JoinHandle<()>,
}

impl InitializationFaultProxy {
    pub async fn start(options: MySqlConnectOptions) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test proxy");
        let port = listener.local_addr().unwrap().port();
        let backend = (options.get_host().to_owned(), options.get_port());
        let control = Arc::new(Control::default());
        let shared = control.clone();
        let task = tokio::spawn(async move {
            let mut sessions = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (client, _) = accepted.expect("accept synthetic test connection");
                        let server = TcpStream::connect(backend.clone()).await.expect("connect test backend");
                        sessions.spawn(session(client, server, shared.clone()));
                    }
                    result = sessions.join_next(), if !sessions.is_empty() => {
                        result.unwrap().expect("fault proxy session");
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

    pub fn pause_ddl_response(&self, verb: &'static str, fragment: &'static str) {
        let release = Arc::new(Notify::new());
        *self.control.release.lock().unwrap() = Some(release.clone());
        *self.control.pause.lock().unwrap() = Some(Pause {
            verb,
            fragment,
            release,
        });
    }

    pub async fn ddl_committed_with_reply_held(&self) {
        tokio::time::timeout(Duration::from_secs(15), self.control.paused.notified())
            .await
            .expect("real DDL must return OK at the proxy");
        assert!(!self.task.is_finished(), "fault proxy must remain alive");
    }

    pub fn release_ddl_response(&self) {
        if let Some(release) = self.control.release.lock().unwrap().take() {
            release.notify_one();
        }
    }

    pub fn reject_initialization_and_cleanup(&self) {
        self.control.reject_init.store(true, Ordering::SeqCst);
        self.control.reject_cleanup.store(true, Ordering::SeqCst);
    }

    pub fn reject_init_and_pause_cleanup(&self) {
        self.control.reject_init.store(true, Ordering::SeqCst);
        self.control.pause_cleanup.store(true, Ordering::SeqCst);
    }

    pub async fn cleanup_is_paused(&self) {
        tokio::time::timeout(
            Duration::from_secs(15),
            self.control.cleanup_paused.notified(),
        )
        .await
        .expect("compensating cleanup reaches proxy");
    }

    pub fn resume_cleanup(&self) {
        self.control.resume_cleanup.notify_one();
    }

    pub fn cleanup_rejections(&self) -> usize {
        self.control.cleanup_rejections.load(Ordering::SeqCst)
    }
}

impl Drop for InitializationFaultProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn server_error(sequence: u8, reason: &'static [u8]) -> Vec<u8> {
    let mut payload = vec![0xff];
    payload.extend_from_slice(&1105_u16.to_le_bytes()); // ER_UNKNOWN_ERROR
    payload.extend_from_slice(b"#HY000");
    payload.extend_from_slice(reason);
    let mut bytes = vec![payload.len() as u8, 0, 0, sequence];
    bytes.extend_from_slice(&payload);
    bytes
}

async fn session(client: TcpStream, server: TcpStream, control: Arc<Control>) {
    let (mut cr, cw) = client.into_split();
    let cw = Arc::new(tokio::sync::Mutex::new(cw));
    let (mut sr, mut sw) = server.into_split();
    let pending_reply = Arc::new(Mutex::new(None::<Arc<Notify>>));
    let requests_pending = pending_reply.clone();
    let preparing = Arc::new(Mutex::new(None::<Arc<Notify>>));
    let requests_preparing = preparing.clone();
    let prepared = Arc::new(Mutex::new(None::<(u32, Arc<Notify>)>));
    let requests_prepared = prepared.clone();
    let requests_control = control.clone();
    let requests_output = cw.clone();
    let requests = async move {
        while let Ok(bytes) = packet(&mut cr).await {
            if bytes.len() > 5 && bytes[3] == 0 && matches!(bytes[4], COM_QUERY | COM_STMT_PREPARE)
            {
                let sql = String::from_utf8_lossy(&bytes[5..]).to_ascii_lowercase();
                let failure = if sql.starts_with("create table")
                    && sql.contains("mem_memories")
                    && requests_control.reject_init.swap(false, Ordering::SeqCst)
                {
                    Some(b"synthetic initialization failure".as_slice())
                } else if sql.starts_with("delete from mem_user_registry")
                    && requests_control
                        .reject_cleanup
                        .swap(false, Ordering::SeqCst)
                {
                    requests_control
                        .cleanup_rejections
                        .fetch_add(1, Ordering::SeqCst);
                    Some(b"synthetic cleanup failure".as_slice())
                } else {
                    None
                };
                if let Some(reason) = failure {
                    if requests_output
                        .lock()
                        .await
                        .write_all(&server_error(bytes[3] + 1, reason))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }
                if sql.starts_with("delete from mem_user_registry")
                    && requests_control.pause_cleanup.swap(false, Ordering::SeqCst)
                {
                    requests_control.cleanup_paused.notify_one();
                    requests_control.resume_cleanup.notified().await;
                }
                let pause = {
                    let mut armed = requests_control.pause.lock().unwrap();
                    if armed
                        .as_ref()
                        .is_some_and(|p| sql.starts_with(p.verb) && sql.contains(p.fragment))
                    {
                        armed.take()
                    } else {
                        None
                    }
                };
                if let Some(pause) = pause {
                    if bytes[4] == COM_QUERY {
                        *requests_pending.lock().unwrap() = Some(pause.release);
                    } else {
                        // PREPARE's OK only allocates a statement. Capture its
                        // ID and wait for the matching EXECUTE before pausing.
                        *requests_preparing.lock().unwrap() = Some(pause.release);
                    }
                }
            }
            if bytes.len() >= 9 && bytes[3] == 0 && bytes[4] == COM_STMT_EXECUTE {
                let id = u32::from_le_bytes(bytes[5..9].try_into().unwrap());
                let mut prepared = requests_prepared.lock().unwrap();
                if prepared.as_ref().is_some_and(|(target, _)| *target == id) {
                    let (_, release) = prepared.take().unwrap();
                    *requests_pending.lock().unwrap() = Some(release);
                }
            }
            if sw.write_all(&bytes).await.is_err() {
                break;
            }
        }
    };
    let replies = async move {
        while let Ok(bytes) = packet(&mut sr).await {
            let prepare = preparing.lock().unwrap().take();
            if let Some(release) = prepare {
                assert!(
                    bytes.len() >= 16 && bytes[4] == 0x00,
                    "DDL PREPARE must succeed"
                );
                assert_eq!(
                    &bytes[9..13],
                    &[0, 0, 0, 0],
                    "synthetic DDL has no result columns or parameters"
                );
                let id = u32::from_le_bytes(bytes[5..9].try_into().unwrap());
                *prepared.lock().unwrap() = Some((id, release));
            }
            let pause = pending_reply.lock().unwrap().take();
            if let Some(release) = pause {
                assert_eq!(
                    bytes[4], 0x00,
                    "intercepted DDL must really succeed on the backend"
                );
                control.paused.notify_one();
                release.notified().await;
            }
            if cw.lock().await.write_all(&bytes).await.is_err() {
                break;
            }
        }
    };
    tokio::select! { _ = requests => {}, _ = replies => {} }
}
