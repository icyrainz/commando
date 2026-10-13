use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use std::cell::RefCell;

use rand::Rng as _;
use tokio::sync::Notify;
use tracing::info;

pub fn generate_token() -> String {
    let bytes = rand::rng().random::<[u8; 16]>();
    hex::encode(bytes)
}

/// Last 8 characters of a token, for log lines.
pub fn token_suffix(token: &str) -> &str {
    match token.char_indices().rev().nth(7) {
        Some((i, _)) => &token[i..],
        None => token,
    }
}

pub struct StreamExecResult {
    pub exit_code: i32,
    pub duration_ms: u64,
    pub timed_out: bool,
    /// The agent lost output at the command deadline.
    pub truncated: bool,
    /// RPC failure message (connect, auth, deadline, agent error).
    pub error: Option<String>,
}

/// RPC-internal timing breakdown, stored in the session for profiling.
#[derive(Clone, Default)]
pub struct RpcProfile {
    pub tcp_connect_ms: f64,
    pub auth_ms: f64,
    pub exec_rpc_ms: f64,
    /// JSON-encoded agent-side profile (empty if agent doesn't support it).
    pub agent_profile_json: String,
}

pub struct Session {
    stdout_buffer: VecDeque<u8>,
    stderr_buffer: VecDeque<u8>,
    pub completed: bool,
    pub exec_result: Option<StreamExecResult>,
    pub rpc_profile: Option<RpcProfile>,
    pub last_polled: Instant,
    pub notify: Rc<Notify>,
    /// Woken when a page drains the buffers or the session ends; blocked `receive` calls wait on it.
    pub drained: Rc<Notify>,
    /// `receive` calls currently held back by the buffer cap.
    pub held_receives: usize,
    /// When the last held `receive` call was answered or dropped.
    pub last_hold_end: Option<Instant>,
    pub rpc_task: Option<tokio::task::JoinHandle<()>>,
}

impl Session {
    fn new() -> Self {
        Self {
            stdout_buffer: VecDeque::new(),
            stderr_buffer: VecDeque::new(),
            completed: false,
            exec_result: None,
            rpc_profile: None,
            last_polled: Instant::now(),
            notify: Rc::new(Notify::new()),
            drained: Rc::new(Notify::new()),
            held_receives: 0,
            last_hold_end: None,
            rpc_task: None,
        }
    }

    pub fn push_stdout(&mut self, data: &[u8]) {
        self.stdout_buffer.extend(data);
    }

    pub fn push_stderr(&mut self, data: &[u8]) {
        self.stderr_buffer.extend(data);
    }

    pub fn drain_stdout(&mut self) -> Vec<u8> {
        self.stdout_buffer.drain(..).collect()
    }

    pub fn drain_stderr(&mut self) -> Vec<u8> {
        self.stderr_buffer.drain(..).collect()
    }

    /// Remove up to `max_bytes` from the front of stdout. Cost is O(returned bytes).
    pub fn drain_stdout_up_to(&mut self, max_bytes: usize) -> Vec<u8> {
        let n = max_bytes.min(self.stdout_buffer.len());
        self.stdout_buffer.drain(..n).collect()
    }

    /// Remove up to `max_bytes` from the front of stderr. Cost is O(returned bytes).
    pub fn drain_stderr_up_to(&mut self, max_bytes: usize) -> Vec<u8> {
        let n = max_bytes.min(self.stderr_buffer.len());
        self.stderr_buffer.drain(..n).collect()
    }

    /// Return bytes to the front of stdout (undo part of a drain).
    pub fn put_back_stdout(&mut self, bytes: &[u8]) {
        for &b in bytes.iter().rev() {
            self.stdout_buffer.push_front(b);
        }
    }

    /// Return bytes to the front of stderr (undo part of a drain).
    pub fn put_back_stderr(&mut self, bytes: &[u8]) {
        for &b in bytes.iter().rev() {
            self.stderr_buffer.push_front(b);
        }
    }

    pub fn stdout_buffered(&self) -> usize {
        self.stdout_buffer.len()
    }

    pub fn stderr_buffered(&self) -> usize {
        self.stderr_buffer.len()
    }

    pub fn touch(&mut self) {
        self.last_polled = Instant::now();
    }

    pub fn total_buffered(&self) -> usize {
        self.stdout_buffer.len() + self.stderr_buffer.len()
    }
}

pub struct SessionMap {
    sessions: HashMap<String, Session>,
    token_to_session: HashMap<String, String>,
}

impl SessionMap {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            token_to_session: HashMap::new(),
        }
    }

    pub fn create_session(&mut self) -> (String, String) {
        let token = generate_token();
        let session_id = generate_token();
        self.sessions.insert(session_id.clone(), Session::new());
        self.token_to_session
            .insert(token.clone(), session_id.clone());
        (token, session_id)
    }

    pub fn get_by_token(&self, token: &str) -> Option<&Session> {
        let session_id = self.token_to_session.get(token)?;
        self.sessions.get(session_id)
    }

    pub fn get_by_token_mut(&mut self, token: &str) -> Option<&mut Session> {
        let session_id = self.token_to_session.get(token)?.clone();
        self.sessions.get_mut(&session_id)
    }

    pub fn rotate_token(&mut self, old_token: &str) -> Option<String> {
        let session_id = self.token_to_session.remove(old_token)?;
        let new_token = generate_token();
        self.token_to_session.insert(new_token.clone(), session_id);
        Some(new_token)
    }

    pub fn remove_by_token(&mut self, token: &str) -> Option<Session> {
        let session_id = self.token_to_session.remove(token)?;
        self.sessions.remove(&session_id)
    }

    pub fn get_by_id(&self, session_id: &str) -> Option<&Session> {
        self.sessions.get(session_id)
    }

    pub fn get_by_id_mut(&mut self, session_id: &str) -> Option<&mut Session> {
        self.sessions.get_mut(session_id)
    }

    pub fn session_id_for_token(&self, token: &str) -> Option<&str> {
        self.token_to_session.get(token).map(String::as_str)
    }

    /// Remove the session behind `token` and abort its RPC task, which closes
    /// the agent connection and kills the command. Returns whether one existed.
    pub fn cancel_by_token(&mut self, token: &str) -> bool {
        match self.remove_by_token(token) {
            Some(mut session) => {
                // Wake pages waiting on this session so they fail now, not at their deadline.
                session.notify.notify_waiters();
                session.drained.notify_waiters();
                if let Some(task) = session.rpc_task.take() {
                    task.abort();
                }
                true
            }
            None => false,
        }
    }

    pub fn cleanup_expired(&mut self, idle_timeout: Duration) -> Vec<String> {
        let now = Instant::now();
        let expired_ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, session)| now.duration_since(session.last_polled) >= idle_timeout)
            .map(|(id, _)| id.clone())
            .collect();

        for session_id in &expired_ids {
            if let Some(mut session) = self.sessions.remove(session_id) {
                session.notify.notify_waiters();
                session.drained.notify_waiters();
                if let Some(task) = session.rpc_task.take() {
                    task.abort();
                }
            }
        }

        // Remove all token mappings pointing to expired session IDs
        self.token_to_session
            .retain(|_, sid| !expired_ids.contains(sid));

        expired_ids
    }
}

/// Spawn the timer that cancels sessions not polled for `idle_timeout`.
/// Must run inside a `LocalSet`.
pub fn spawn_idle_cleanup(session_map: Rc<RefCell<SessionMap>>, idle_timeout: Duration) {
    tokio::task::spawn_local(async move {
        let period = (idle_timeout / 2).clamp(Duration::from_secs(1), Duration::from_secs(10));
        let mut interval = tokio::time::interval(period);
        loop {
            interval.tick().await;
            let expired = session_map.borrow_mut().cleanup_expired(idle_timeout);
            if !expired.is_empty() {
                info!(
                    count = expired.len(),
                    "cleaned up expired streaming sessions"
                );
            }
        }
    });
}

impl Default for SessionMap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_map_create_and_lookup() {
        let mut map = SessionMap::new();
        let (token, _) = map.create_session();
        assert!(map.get_by_token(&token).is_some());
        assert!(map.get_by_token("bogus").is_none());
    }

    #[test]
    fn session_token_rotation() {
        let mut map = SessionMap::new();
        let (token1, _) = map.create_session();
        let token2 = map.rotate_token(&token1).unwrap();
        assert_ne!(token1, token2);
        assert!(map.get_by_token(&token1).is_none());
        assert!(map.get_by_token(&token2).is_some());
    }

    #[test]
    fn session_drain_stdout() {
        let mut map = SessionMap::new();
        let (token, _) = map.create_session();
        map.get_by_token_mut(&token)
            .unwrap()
            .push_stdout(b"hello world");
        let drained = map.get_by_token_mut(&token).unwrap().drain_stdout();
        assert_eq!(drained, b"hello world");
        assert_eq!(map.get_by_token_mut(&token).unwrap().stdout_buffered(), 0);
    }

    #[test]
    fn session_drain_stderr() {
        let mut map = SessionMap::new();
        let (token, _) = map.create_session();
        map.get_by_token_mut(&token)
            .unwrap()
            .push_stderr(b"err msg");
        let drained = map.get_by_token_mut(&token).unwrap().drain_stderr();
        assert_eq!(drained, b"err msg");
    }

    #[test]
    fn session_cleanup_expired() {
        let mut map = SessionMap::new();
        let (token, _) = map.create_session();
        map.get_by_token_mut(&token).unwrap().last_polled =
            Instant::now() - Duration::from_secs(120);
        let expired = map.cleanup_expired(Duration::from_secs(60));
        assert_eq!(expired.len(), 1);
        assert!(map.get_by_token(&token).is_none());
    }

    #[test]
    fn session_cancel_by_token_removes_session() {
        let mut map = SessionMap::new();
        let (token, _) = map.create_session();
        assert!(map.cancel_by_token(&token));
        assert!(map.get_by_token(&token).is_none());
        assert!(!map.cancel_by_token(&token));
    }

    #[test]
    fn token_suffix_is_safe_for_any_string() {
        assert_eq!(token_suffix("0123456789abcdef"), "89abcdef");
        assert_eq!(token_suffix("short"), "short");
        assert_eq!(token_suffix(""), "");
        assert_eq!(token_suffix("ééééééééé"), "éééééééé");
        assert_eq!(token_suffix("日本語"), "日本語");
    }

    #[test]
    fn generate_token_is_unique() {
        let t1 = generate_token();
        let t2 = generate_token();
        assert_ne!(t1, t2);
        assert_eq!(t1.len(), 32); // 128-bit = 16 bytes = 32 hex chars
    }

    #[test]
    fn session_drain_up_to() {
        let mut map = SessionMap::new();
        let (token, _) = map.create_session();
        let session = map.get_by_token_mut(&token).unwrap();
        session.push_stdout(b"hello world, this is a longer message");
        let drained = session.drain_stdout_up_to(11);
        assert_eq!(drained, b"hello world");
        assert_eq!(session.drain_stdout(), b", this is a longer message");
    }

    #[test]
    fn session_put_back_restores_front() {
        let mut map = SessionMap::new();
        let (token, _) = map.create_session();
        let session = map.get_by_token_mut(&token).unwrap();
        session.push_stdout(b"abcdef");
        let drained = session.drain_stdout_up_to(4);
        session.put_back_stdout(&drained[2..]);
        session.push_stdout(b"gh");
        assert_eq!(session.drain_stdout(), b"cdefgh");
    }
}
