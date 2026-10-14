// SPDX-License-Identifier: GPL-3.0-only
//! The relay: RTMP listener(s), route lookup, the set of live sessions,
//! graceful shutdown and emergency stop.

use crate::engine::Engine;
use crate::routes::{Route, normalize_path};
use crate::session::{self, Session};
use crate::upstream;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio_rustls::rustls::ClientConfig;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub struct RelayOptions {
    /// Folder for per-session logs (`None`: no log files).
    pub log_dir: Option<PathBuf>,
    /// TLS client configuration for `rtmps` upstreams (default: OS trust store).
    pub tls: Option<Arc<ClientConfig>>,
}

pub(crate) struct Shared {
    routes: RwLock<Vec<Route>>,
    sessions: Mutex<Vec<Arc<Session>>>,
    /// Cancelled when the relay stops accepting connections (shutdown).
    stop: CancellationToken,
    /// Latched by emergency stop: refuse publishing until cleared.
    emergency: AtomicBool,
    next_id: AtomicU64,
    pub tls: Arc<ClientConfig>,
    pub started: Instant,
    pub log_dir: Option<PathBuf>,
}

impl Shared {
    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Session>>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn route_for(&self, path: &str) -> Result<Route, String> {
        if self.emergency.load(Ordering::SeqCst) {
            return Err("Emergency stop is active in OpenDelay; allow streaming again in the app first.".into());
        }
        if self.stop.is_cancelled() {
            return Err("OpenDelay is shutting down.".into());
        }
        let routes = self.routes.read().unwrap_or_else(|e| e.into_inner());
        match routes.iter().find(|r| normalize_path(&r.path) == path) {
            Some(r) if r.enabled => Ok(r.clone()),
            Some(r) => Err(format!("The route \"{}\" is disabled in OpenDelay.", r.name)),
            None => Err(format!("OpenDelay has no route \"/{path}\". Check the server URL in your encoder.")),
        }
    }

    pub fn new_engine(&self) -> Engine {
        Engine::new()
    }

    pub fn add_session(&self, s: Arc<Session>) {
        self.lock_sessions().push(s);
    }

    pub fn remove_session(&self, id: u64) {
        self.lock_sessions().retain(|s| s.id != id);
    }

    /// Stop sessions already publishing on `path` and wait (≤ 3 s) for them to end.
    pub async fn replace_route_sessions(&self, path: &str) {
        let old: Vec<Arc<Session>> = self
            .lock_sessions()
            .iter()
            .filter(|s| normalize_path(&s.route.path) == normalize_path(path))
            .cloned()
            .collect();
        for s in &old {
            s.log.event("Replaced by a new encoder connection on the same route");
            s.token.cancel();
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && self.lock_sessions().iter().any(|s| old.iter().any(|o| o.id == s.id)) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[derive(Clone)]
pub struct Relay {
    shared: Arc<Shared>,
}

impl Relay {
    pub fn new(routes: Vec<Route>, opts: RelayOptions) -> Self {
        Relay {
            shared: Arc::new(Shared {
                routes: RwLock::new(routes),
                sessions: Mutex::new(Vec::new()),
                stop: CancellationToken::new(),
                emergency: AtomicBool::new(false),
                next_id: AtomicU64::new(1),
                tls: opts.tls.unwrap_or_else(upstream::default_tls),
                started: Instant::now(),
                log_dir: opts.log_dir,
            }),
        }
    }

    /// Bind an RTMP listener and accept encoders until shutdown.
    pub async fn listen(&self, addr: SocketAddr) -> io::Result<SocketAddr> {
        let listener = TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        let shared = self.shared.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shared.stop.cancelled() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((tcp, peer)) => {
                            let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
                            tokio::spawn(session::run(shared.clone(), tcp, peer, id));
                        }
                        // e.g. out of file descriptors: back off briefly
                        Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                    },
                }
            }
        });
        Ok(local)
    }

    pub fn routes(&self) -> Vec<Route> {
        self.shared.routes.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Replace the route table; takes effect on the next encoder connection.
    pub fn set_routes(&self, routes: Vec<Route>) {
        *self.shared.routes.write().unwrap_or_else(|e| e.into_inner()) = routes;
    }

    pub fn sessions(&self) -> Vec<Arc<Session>> {
        self.shared.lock_sessions().clone()
    }

    /// Disconnect every stream (encoder and upstream) at once, and refuse new
    /// publishes until [`Relay::clear_emergency`].
    pub fn emergency_stop(&self) {
        self.shared.emergency.store(true, Ordering::SeqCst);
        for s in self.sessions() {
            s.log.event("Emergency stop");
            s.token.cancel();
        }
    }

    pub fn clear_emergency(&self) {
        self.shared.emergency.store(false, Ordering::SeqCst);
    }

    pub fn emergency_active(&self) -> bool {
        self.shared.emergency.load(Ordering::SeqCst)
    }

    /// Stop accepting encoders, wait up to `wait` for active streams to end,
    /// then disconnect whatever is left.
    pub async fn shutdown(&self, wait: Duration) {
        self.shared.stop.cancel();
        let deadline = Instant::now() + wait;
        while !self.sessions().is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        for s in self.sessions() {
            s.token.cancel();
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while !self.sessions().is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
